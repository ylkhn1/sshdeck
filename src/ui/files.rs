//! Панель файлов: дерево удалённого каталога, операции с файлами, передачи
//! с прогрессом, правка файлов в локальном редакторе.
//!
//! Панель ничего не делает с сетью сама: она отправляет команды фоновой
//! SFTP-задаче и обновляет виджеты по её событиям.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use adw::prelude::*;
use gtk::{gdk, gio, glib, graphene, pango};

use super::dnd::RemoteFilesProvider;
use super::{
    ANIMATION_MS, Notifier, collapsing_revealer, dialogs, format_mtime, format_size,
    parent_window, reveal,
};
use crate::session::{self, Session};
use crate::sftp::{
    self, Command, ConnState, EntryKind, Event, RemoteEntry, SftpWorker, TransferId,
    TransferOutcome,
};

/// Файлы крупнее этого открываются в редакторе только после подтверждения.
const LARGE_EDIT_BYTES: u64 = 50 * 1024 * 1024;
/// Пауза после последнего изменения файла перед заливкой на сервер:
/// редакторы сохраняют в несколько приёмов.
const EDIT_DEBOUNCE: Duration = Duration::from_millis(400);

static NEXT_TRANSFER: AtomicU64 = AtomicU64::new(1);

type DoneCallback = Box<dyn FnOnce(&TransferOutcome)>;
/// Время изменения и размер локального файла.
type Stamp = (Option<SystemTime>, u64);

#[derive(Clone)]
pub struct FilePanel {
    inner: Rc<Inner>,
}

/// Просьба терминалу перейти в каталог.
type CdHandler = Box<dyn Fn(&str)>;

struct Inner {
    root: gtk::Box,
    toolbar: gtk::Box,
    path_entry: gtk::Entry,
    stack: gtk::Stack,
    status_page: adw::StatusPage,
    spinner: adw::Spinner,
    view: gtk::ColumnView,
    selection: gtk::MultiSelection,
    root_store: gio::ListStore,
    context_menu: gtk::PopoverMenu,
    transfers_box: gtk::Box,
    /// Выезжает снизу, пока есть активные передачи.
    transfers_revealer: gtk::Revealer,
    /// Уведомления вкладки (общие с терминалом).
    notifier: Notifier,
    /// Каталог вкладки для временных файлов (правка, перетаскивание).
    scratch_dir: PathBuf,
    worker: RefCell<Option<SftpWorker>>,
    /// Работает ли ssh в терминале вкладки.
    ssh_running: Cell<bool>,
    /// Переходить ли вслед за `cd` в терминале.
    follow: Cell<bool>,
    /// Вызывается каждый раз, когда SFTP-соединение установлено.
    on_connected: RefCell<Option<Box<dyn Fn()>>>,
    /// Просит терминал вкладки перейти в каталог.
    on_terminal_cd: RefCell<Option<CdHandler>>,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    home: String,
    /// Каталог, показанный в корне дерева.
    cwd: String,
    /// Последнее состояние соединения, о котором сообщила SFTP-задача.
    last_conn: Option<ConnState>,
    show_hidden: bool,
    /// Загруженные каталоги: корень и раскрытые узлы.
    dirs: HashMap<String, DirNode>,
    transfers: HashMap<TransferId, TransferRow>,
    edits: Vec<Rc<EditWatch>>,
    scratch_seq: u32,
}

struct DirNode {
    /// Слабая ссылка: модель узла принадлежит дереву и исчезает при
    /// сворачивании.
    store: glib::WeakRef<gio::ListStore>,
    /// Содержимое как пришло с сервера (без фильтра скрытых).
    entries: Vec<RemoteEntry>,
}

struct TransferRow {
    widget: gtk::Box,
    bar: gtk::ProgressBar,
    title: String,
    /// Удалённый каталог, который нужно перечитать после успеха.
    refresh: Option<String>,
    on_done: Option<DoneCallback>,
}

/// Файл, открытый в локальном редакторе.
struct EditWatch {
    remote: String,
    local: PathBuf,
    /// Держит наблюдение за каталогом с копией.
    monitor: Option<gio::FileMonitor>,
    /// Состояние файла, которое уже отправлено на сервер.
    stamp: Cell<Option<Stamp>>,
    uploading: Cell<bool>,
    /// Файл изменился, пока шла предыдущая заливка.
    dirty: Cell<bool>,
    generation: Cell<u32>,
}

impl FilePanel {
    pub fn new(session: &Session, scratch_dir: PathBuf, notifier: Notifier) -> Self {
        let inner = Rc::new_cyclic(|weak| Inner::build(weak, scratch_dir, notifier));

        let (tx, rx) = async_channel::unbounded();
        match SftpWorker::start(session.clone(), tx) {
            Ok(worker) => *inner.worker.borrow_mut() = Some(worker),
            Err(err) => inner.show_status(
                false,
                tr!("Could Not Start SFTP"),
                &err.to_string(),
            ),
        }
        let weak = Rc::downgrade(&inner);
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                inner.handle_event(event);
            }
        });
        Self { inner }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.inner.root
    }

    /// Сообщает панели, работает ли ssh в терминале вкладки.
    pub fn set_ssh_running(&self, running: bool) {
        self.inner.set_ssh_running(running);
    }

    /// Терминал сообщил свой текущий каталог: панель переходит туда же,
    /// если слежение включено и соединение установлено.
    pub fn follow_dir(&self, path: &str) {
        let inner = &self.inner;
        let ready = {
            let state = inner.state.borrow();
            matches!(state.last_conn, Some(ConnState::Connected { .. })) && state.cwd != path
        };
        if ready && inner.follow.get() && path.starts_with('/') {
            inner.navigate(path);
        }
    }

    pub fn set_connected_handler(&self, handler: impl Fn() + 'static) {
        *self.inner.on_connected.borrow_mut() = Some(Box::new(handler));
    }

    pub fn set_terminal_cd_handler(&self, handler: impl Fn(&str) + 'static) {
        *self.inner.on_terminal_cd.borrow_mut() = Some(Box::new(handler));
    }

    /// Останавливает SFTP-задачу и удаляет временные файлы вкладки.
    pub fn shutdown(&self) {
        let inner = &self.inner;
        inner.worker.borrow_mut().take();
        let edits = std::mem::take(&mut inner.state.borrow_mut().edits);
        for watch in edits {
            if let Some(monitor) = &watch.monitor {
                monitor.cancel();
            }
        }
        inner.context_menu.unparent();
        if inner.scratch_dir.exists() {
            let _ = std::fs::remove_dir_all(&inner.scratch_dir);
        }
    }
}

// ------------------------------------------------------------ построение

impl Inner {
    fn build(weak: &Weak<Inner>, scratch_dir: PathBuf, notifier: Notifier) -> Inner {
        // --- панель инструментов
        let home_button = icon_button("go-home-symbolic", tr!("Home folder"));
        let up_button = icon_button("go-up-symbolic", tr!("Up one level"));
        let refresh_button = icon_button("view-refresh-symbolic", tr!("Refresh (F5)"));
        refresh_button.set_action_name(Some("files.refresh"));
        let path_entry = gtk::Entry::builder()
            .hexpand(true)
            .tooltip_text(tr!("Current folder. Type a path and press Enter"))
            .build();
        // Класс `toolbar` даёт плоские кнопки и стандартные отступы.
        let nav_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        nav_row.add_css_class("toolbar");
        nav_row.append(&up_button);
        nav_row.append(&home_button);
        nav_row.append(&path_entry);
        nav_row.append(&refresh_button);

        let upload_menu = gio::Menu::new();
        upload_menu.append(Some(tr!("Files…")), Some("files.upload-files"));
        upload_menu.append(Some(tr!("Folder…")), Some("files.upload-folder"));
        let upload_button = gtk::MenuButton::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("document-send-symbolic")
                    .label(tr!("Upload"))
                    .build(),
            )
            .always_show_arrow(true)
            .tooltip_text(tr!("Upload to the current folder on the server"))
            .menu_model(&upload_menu)
            .build();
        let download_button = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("folder-download-symbolic")
                    .label(tr!("Download"))
                    .build(),
            )
            .tooltip_text(tr!("Download the selection to a local folder"))
            .action_name("files.download")
            .build();
        let mkdir_button = icon_button("folder-new-symbolic", tr!("New folder"));
        mkdir_button.set_action_name(Some("files.mkdir"));
        let hidden_toggle = gtk::ToggleButton::builder()
            .icon_name("view-reveal-symbolic")
            .tooltip_text(tr!("Show hidden files"))
            .build();
        let follow_toggle = gtk::ToggleButton::builder()
            .icon_name("media-playlist-consecutive-symbolic")
            .tooltip_text(tr!("Follow cd in the terminal"))
            .active(true)
            .build();
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        let ops_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        ops_row.add_css_class("toolbar");
        ops_row.append(&upload_button);
        ops_row.append(&download_button);
        ops_row.append(&spacer);
        ops_row.append(&mkdir_button);
        ops_row.append(&follow_toggle);
        ops_row.append(&hidden_toggle);

        let toolbar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        toolbar.add_css_class("sshdeck-files-toolbar");
        toolbar.append(&nav_row);
        toolbar.append(&ops_row);
        toolbar.set_sensitive(false);

        // --- дерево файлов
        let root_store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let tree = {
            let weak = weak.clone();
            gtk::TreeListModel::new(root_store.clone(), false, false, move |item| {
                let entry = item
                    .downcast_ref::<glib::BoxedAnyObject>()?
                    .borrow::<RemoteEntry>()
                    .clone();
                if entry.kind != EntryKind::Dir {
                    return None;
                }
                let store = gio::ListStore::new::<glib::BoxedAnyObject>();
                // GTK вызывает эту функцию и просто чтобы узнать, раскрываем
                // ли узел, сразу выбрасывая модель. Каталог читается, только
                // если модель дожила до следующей итерации цикла, то есть
                // узел действительно раскрыт.
                let weak_store = store.downgrade();
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let (Some(inner), Some(store)) = (weak.upgrade(), weak_store.upgrade()) {
                        inner.load_dir(&entry.path, &store);
                    }
                });
                Some(store.upcast())
            })
        };
        let selection = gtk::MultiSelection::new(Some(tree));
        let view = gtk::ColumnView::builder()
            .model(&selection)
            .show_row_separators(false)
            .reorderable(false)
            .build();
        view.add_css_class("sshdeck-files");
        let name_column = name_column(weak);
        name_column.set_expand(true);
        name_column.set_resizable(true);
        view.append_column(&name_column);
        let size_column = text_column(tr!("Size"), weak, 1.0, |entry| match entry.kind {
            EntryKind::File => entry.size.map(format_size).unwrap_or_default(),
            _ => String::new(),
        });
        size_column.set_resizable(true);
        view.append_column(&size_column);
        let mtime_column = text_column(tr!("Modified"), weak, 0.0, |entry| {
            entry.modified.map(format_mtime).unwrap_or_default()
        });
        mtime_column.set_resizable(true);
        view.append_column(&mtime_column);

        let scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hexpand(true)
            .build();

        // --- страница состояния
        let spinner = adw::Spinner::new();
        spinner.set_size_request(32, 32);
        spinner.set_halign(gtk::Align::Center);
        let status_page = adw::StatusPage::builder()
            .child(&spinner)
            .css_classes(["compact"])
            .build();

        let stack = gtk::Stack::new();
        stack.add_named(&status_page, Some("status"));
        stack.add_named(&scroll, Some("files"));
        stack.set_vexpand(true);
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_transition_duration(ANIMATION_MS);

        // --- контекстное меню
        let menu = gio::Menu::new();
        let open_section = gio::Menu::new();
        open_section.append(Some(tr!("Open or Edit")), Some("files.open"));
        open_section.append(Some(tr!("Download…")), Some("files.download"));
        open_section.append(Some(tr!("Go Here in Terminal")), Some("files.cd-terminal"));
        open_section.append(Some(tr!("Copy Path")), Some("files.copy-path"));
        menu.append_section(None, &open_section);
        let create_section = gio::Menu::new();
        create_section.append(Some(tr!("Upload Files…")), Some("files.upload-files"));
        create_section.append(Some(tr!("Upload Folder…")), Some("files.upload-folder"));
        create_section.append(Some(tr!("New Folder…")), Some("files.mkdir"));
        menu.append_section(None, &create_section);
        let change_section = gio::Menu::new();
        change_section.append(Some(tr!("Rename…")), Some("files.rename"));
        change_section.append(Some(tr!("Delete")), Some("files.delete"));
        menu.append_section(None, &change_section);
        let refresh_section = gio::Menu::new();
        refresh_section.append(Some(tr!("Refresh")), Some("files.refresh"));
        menu.append_section(None, &refresh_section);
        let context_menu = gtk::PopoverMenu::from_model(Some(&menu));
        context_menu.set_parent(&view);
        context_menu.set_has_arrow(false);
        context_menu.set_halign(gtk::Align::Start);

        // Показывается, только пока есть активные передачи.
        let transfers_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        transfers_box.add_css_class("sshdeck-transfers");
        let transfers_revealer =
            collapsing_revealer(gtk::RevealerTransitionType::SlideUp, &transfers_box);
        transfers_revealer.set_visible(false);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_width_request(300);
        root.append(&toolbar);
        root.append(&stack);
        root.append(&transfers_revealer);

        // --- действия
        let actions = gio::SimpleActionGroup::new();
        add_action(&actions, "refresh", weak, |inner| inner.refresh_all());
        add_action(&actions, "open", weak, |inner| inner.open_selected());
        add_action(&actions, "download", weak, |inner| inner.download_selected());
        add_action(&actions, "upload-files", weak, |inner| inner.choose_upload(false));
        add_action(&actions, "upload-folder", weak, |inner| inner.choose_upload(true));
        add_action(&actions, "mkdir", weak, |inner| inner.make_dir());
        add_action(&actions, "rename", weak, |inner| inner.rename_selected());
        add_action(&actions, "delete", weak, |inner| inner.delete_selected());
        add_action(&actions, "cd-terminal", weak, |inner| inner.cd_terminal());
        add_action(&actions, "copy-path", weak, |inner| inner.copy_path());
        root.insert_action_group("files", Some(&actions));

        let shortcuts = gtk::ShortcutController::new();
        for (trigger, action) in [
            ("F5", "files.refresh"),
            ("F2", "files.rename"),
            ("Delete", "files.delete"),
        ] {
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(trigger),
                Some(gtk::NamedAction::new(action)),
            ));
        }
        view.add_controller(shortcuts);

        // --- обработчики
        connect(weak, |weak| {
            home_button.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    let home = inner.state.borrow().home.clone();
                    inner.navigate(&home);
                }
            });
        });
        connect(weak, |weak| {
            up_button.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    let parent = sftp::parent_remote(&inner.state.borrow().cwd);
                    inner.navigate(&parent);
                }
            });
        });
        connect(weak, |weak| {
            path_entry.connect_activate(move |entry| {
                if let Some(inner) = weak.upgrade() {
                    let path = inner.resolve_input(&entry.text());
                    inner.navigate(&path);
                }
            });
        });
        connect(weak, |weak| {
            follow_toggle.connect_toggled(move |toggle| {
                if let Some(inner) = weak.upgrade() {
                    inner.follow.set(toggle.is_active());
                }
            });
        });
        connect(weak, |weak| {
            hidden_toggle.connect_toggled(move |toggle| {
                if let Some(inner) = weak.upgrade() {
                    inner.state.borrow_mut().show_hidden = toggle.is_active();
                    inner.refill_all();
                }
            });
        });
        connect(weak, |weak| {
            view.connect_activate(move |_, position| {
                if let Some(inner) = weak.upgrade()
                    && let Some(entry) = inner.entry_at(position)
                {
                    inner.activate_entry(entry);
                }
            });
        });
        connect(weak, |weak| {
            // Щелчок правой кнопкой по пустому месту: меню для каталога.
            let gesture = gtk::GestureClick::builder()
                .button(gdk::BUTTON_SECONDARY)
                .build();
            gesture.connect_pressed(move |gesture, _, x, y| {
                if let (Some(inner), Some(widget)) = (weak.upgrade(), gesture.widget()) {
                    inner.popup_menu(&widget, x, y);
                }
            });
            scroll.add_controller(gesture);
        });
        connect(weak, |weak| {
            // Приём файлов, перетащенных из файлового менеджера.
            let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
            drop.connect_drop(move |target, value, _, _| {
                // Перетаскивание из самой панели сюда же — не загрузка.
                if target.current_drop().and_then(|d| d.drag()).is_some() {
                    return false;
                }
                let (Some(inner), Ok(list)) = (weak.upgrade(), value.get::<gdk::FileList>())
                else {
                    return false;
                };
                let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
                if paths.is_empty() {
                    return false;
                }
                inner.upload_paths(paths);
                true
            });
            stack.add_controller(drop);
        });

        Inner {
            root,
            toolbar,
            path_entry,
            stack,
            status_page,
            spinner,
            view,
            selection,
            root_store,
            context_menu,
            transfers_box,
            transfers_revealer,
            notifier,
            scratch_dir,
            worker: RefCell::new(None),
            ssh_running: Cell::new(true),
            follow: Cell::new(true),
            on_connected: RefCell::new(None),
            on_terminal_cd: RefCell::new(None),
            state: RefCell::new(State::default()),
        }
    }
}

/// Передаёт обработчику собственную копию слабой ссылки на панель.
fn connect(weak: &Weak<Inner>, wire: impl FnOnce(Weak<Inner>)) {
    wire(weak.clone());
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .build()
}

fn add_action(
    group: &gio::SimpleActionGroup,
    name: &str,
    weak: &Weak<Inner>,
    handler: impl Fn(&Rc<Inner>) + 'static,
) {
    let action = gio::SimpleAction::new(name, None);
    let weak = weak.clone();
    action.connect_activate(move |_, _| {
        if let Some(inner) = weak.upgrade() {
            handler(&inner);
        }
    });
    group.add_action(&action);
}

fn entry_of_item(item: &gtk::ListItem) -> Option<RemoteEntry> {
    let row = item.item().and_downcast::<gtk::TreeListRow>()?;
    let object = row.item().and_downcast::<glib::BoxedAnyObject>()?;
    let entry = object.borrow::<RemoteEntry>().clone();
    Some(entry)
}

fn icon_for(entry: &RemoteEntry) -> &'static str {
    match entry.kind {
        EntryKind::Dir => "folder-symbolic",
        EntryKind::File => "text-x-generic-symbolic",
        EntryKind::Other => "dialog-question-symbolic",
    }
}

/// Столбец имени: стрелка раскрытия, значок и имя.
fn name_column(weak: &Weak<Inner>) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    {
        let weak = weak.clone();
        factory.connect_setup(move |_, object| {
            let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let icon = gtk::Image::new();
            let label = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::Middle)
                .hexpand(true)
                .build();
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            content.append(&icon);
            content.append(&label);
            let expander = gtk::TreeExpander::new();
            expander.set_child(Some(&content));
            attach_context_click(&expander, item, &weak);
            attach_drag_source(&expander, item, &weak);
            item.set_child(Some(&expander));
        });
    }
    factory.connect_bind(|_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else {
            return;
        };
        expander.set_list_row(item.item().and_downcast::<gtk::TreeListRow>().as_ref());
        let (Some(entry), Some(content)) = (entry_of_item(item), expander.child()) else {
            return;
        };
        if let Some(icon) = content.first_child().and_downcast::<gtk::Image>() {
            icon.set_icon_name(Some(icon_for(&entry)));
        }
        if let Some(label) = content.last_child().and_downcast::<gtk::Label>() {
            label.set_text(&entry.name);
            label.set_tooltip_text(match (entry.is_link, entry.kind) {
                (true, EntryKind::Other) => Some(tr!("Broken symbolic link")),
                (true, _) => Some(tr!("Symbolic link")),
                (false, _) => None,
            });
        }
    });
    factory.connect_unbind(|_, object| {
        if let Some(expander) = object
            .downcast_ref::<gtk::ListItem>()
            .and_then(|item| item.child())
            .and_downcast::<gtk::TreeExpander>()
        {
            expander.set_list_row(None);
        }
    });
    gtk::ColumnViewColumn::new(Some(tr!("Name")), Some(factory))
}

fn text_column(
    title: &str,
    weak: &Weak<Inner>,
    xalign: f32,
    text: impl Fn(&RemoteEntry) -> String + 'static,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    {
        let weak = weak.clone();
        factory.connect_setup(move |_, object| {
            let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let label = gtk::Label::builder()
                .xalign(xalign)
                .margin_start(8)
                .margin_end(8)
                .build();
            attach_context_click(&label, item, &weak);
            item.set_child(Some(&label));
        });
    }
    factory.connect_bind(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        if let (Some(label), Some(entry)) =
            (item.child().and_downcast::<gtk::Label>(), entry_of_item(item))
        {
            label.set_text(&text(&entry));
        }
    });
    gtk::ColumnViewColumn::new(Some(title), Some(factory))
}

/// Правая кнопка по строке: выделяет её (если не выделена) и показывает меню.
fn attach_context_click(widget: &impl IsA<gtk::Widget>, item: &gtk::ListItem, weak: &Weak<Inner>) {
    let gesture = gtk::GestureClick::builder()
        .button(gdk::BUTTON_SECONDARY)
        .build();
    let weak = weak.clone();
    let item = item.downgrade();
    gesture.connect_pressed(move |gesture, _, x, y| {
        let (Some(inner), Some(item)) = (weak.upgrade(), item.upgrade()) else {
            return;
        };
        let position = item.position();
        if !inner.selection.is_selected(position) {
            inner.selection.select_item(position, true);
        }
        gesture.set_state(gtk::EventSequenceState::Claimed);
        if let Some(widget) = gesture.widget() {
            inner.popup_menu(&widget, x, y);
        }
    });
    widget.add_controller(gesture);
}

/// Перетаскивание строки наружу: данные скачиваются, когда их запросит
/// приёмник.
fn attach_drag_source(widget: &impl IsA<gtk::Widget>, item: &gtk::ListItem, weak: &Weak<Inner>) {
    let source = gtk::DragSource::builder()
        .actions(gdk::DragAction::COPY)
        .build();
    let weak = weak.clone();
    let item = item.downgrade();
    source.connect_prepare(move |_, _, _| {
        let (inner, item) = (weak.upgrade()?, item.upgrade()?);
        let position = item.position();
        if !inner.selection.is_selected(position) {
            inner.selection.select_item(position, true);
        }
        inner.drag_content()
    });
    widget.add_controller(source);
}

fn sort_key(entry: &RemoteEntry) -> (bool, String, &str) {
    (
        entry.kind != EntryKind::Dir,
        entry.name.to_lowercase(),
        &entry.name,
    )
}

/// Приводит модель к новому списку, трогая только изменившиеся строки:
/// так раскрытые подкаталоги остаются раскрытыми при обновлении.
/// И модель, и `entries` упорядочены по [`sort_key`].
fn sync_store(store: &gio::ListStore, entries: &[RemoteEntry]) {
    let mut position = 0u32;
    let mut index = 0usize;
    loop {
        let existing = store
            .item(position)
            .and_downcast::<glib::BoxedAnyObject>();
        match (existing, entries.get(index)) {
            (None, None) => break,
            (None, Some(_)) => {
                let rest: Vec<glib::BoxedAnyObject> = entries[index..]
                    .iter()
                    .cloned()
                    .map(glib::BoxedAnyObject::new)
                    .collect();
                store.extend_from_slice(&rest);
                break;
            }
            (Some(_), None) => store.remove(position),
            (Some(object), Some(new)) => {
                let (order, replace) = {
                    let old = object.borrow::<RemoteEntry>();
                    let both_dirs = old.kind == EntryKind::Dir && new.kind == EntryKind::Dir;
                    (sort_key(&old).cmp(&sort_key(new)), *old != *new && !both_dirs)
                };
                match order {
                    std::cmp::Ordering::Less => store.remove(position),
                    std::cmp::Ordering::Greater => {
                        store.insert(position, &glib::BoxedAnyObject::new(new.clone()));
                        position += 1;
                        index += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        if replace {
                            store.splice(position, 1, &[glib::BoxedAnyObject::new(new.clone())]);
                        }
                        position += 1;
                        index += 1;
                    }
                }
            }
        }
    }
}

/// Оставляет только верхние элементы: если выбран каталог и что-то внутри
/// него, вложенное отбрасывается.
fn top_level(entries: Vec<RemoteEntry>) -> Vec<RemoteEntry> {
    let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
    entries
        .into_iter()
        .filter(|entry| {
            !paths.iter().any(|other| {
                entry.path.len() > other.len()
                    && entry.path.starts_with(other.as_str())
                    && entry.path.as_bytes()[other.len()] == b'/'
            })
        })
        .collect()
}

/// «a, b, c и ещё 4» — для заголовков и подтверждений.
fn summarize(names: &[String]) -> String {
    const SHOWN: usize = 3;
    let text = names
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        let more = names.len() - SHOWN;
        return trn!("{} and {} more", "{} and {} more", more, text, more);
    }
    text
}

fn file_stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

fn dialog_dismissed(err: &glib::Error) -> bool {
    err.matches(gtk::DialogError::Dismissed) || err.matches(gtk::DialogError::Cancelled)
}

// ------------------------------------------------------------- поведение

impl Inner {
    fn send(&self, command: Command) {
        if let Some(worker) = self.worker.borrow().as_ref() {
            worker.send(command);
        }
    }

    fn window(&self) -> Option<gtk::Window> {
        parent_window(&self.root)
    }

    /// Экран состояния вместо списка; `busy` включает индикатор ожидания.
    fn show_status(&self, busy: bool, title: &str, description: &str) {
        self.status_page.set_title(title);
        // Описание — разметка Pango, поэтому текст экранируется.
        self.status_page
            .set_description(Some(glib::markup_escape_text(description).as_str()));
        self.spinner.set_visible(busy);
        self.stack.set_visible_child_name("status");
        self.toolbar.set_sensitive(false);
    }

    fn handle_event(self: &Rc<Self>, event: Event) {
        match event {
            Event::State(state) => self.on_state(state),
            Event::Navigated { requested, result } => match result {
                Ok((path, entries)) => self.show_dir(path, entries),
                Err(err) => {
                    self.notifier
                        .error(&tr!("Could not open “{}”: {}", requested, err));
                    let cwd = self.state.borrow().cwd.clone();
                    self.path_entry.set_text(&cwd);
                }
            },
            Event::Listing { path, result } => match result {
                Ok(entries) => self.on_listing(&path, entries),
                Err(err) => self
                    .notifier
                    .error(&tr!("Could not read “{}”: {}", path, err)),
            },
            Event::OpDone { error, refresh } => {
                if let Some(error) = error {
                    self.notifier.error(&error);
                }
                for dir in refresh {
                    self.refresh_dir(&dir);
                }
            }
            Event::Progress {
                id,
                done,
                total,
                current,
            } => self.on_progress(id, done, total, &current),
            Event::TransferDone { id, outcome } => self.on_transfer_done(id, outcome),
        }
    }

    /// Терминал сообщает, работает ли в нём ssh: без него ждать входа
    /// бессмысленно, и экран ожидания сменяется на «соединение закрыто».
    fn set_ssh_running(&self, running: bool) {
        self.ssh_running.set(running);
        let last = self.state.borrow().last_conn.clone();
        if let Some(state @ (ConnState::Waiting { .. } | ConnState::Connecting)) = last {
            self.on_state(state);
        }
    }

    fn on_state(&self, state: ConnState) {
        self.state.borrow_mut().last_conn = Some(state.clone());
        match state {
            ConnState::Waiting { .. } if !self.ssh_running.get() => self.show_status(
                false,
                tr!("Connection Closed"),
                tr!("Click “Reconnect” to log in again."),
            ),
            ConnState::Waiting { last_error: None } => self.show_status(
                true,
                tr!("Waiting for Login"),
                tr!(
                    "Enter your password or key passphrase in the terminal on the right — the file \
                     panel will connect automatically."
                ),
            ),
            ConnState::Waiting {
                last_error: Some(error),
            } => self.show_status(
                true,
                tr!("No SFTP Connection"),
                &tr!("{}\nThe panel will connect once you log in in the terminal.", error),
            ),
            ConnState::Connecting => self.show_status(true, tr!("Connecting…"), ""),
            ConnState::Connected { home } => {
                let target = {
                    let mut state = self.state.borrow_mut();
                    state.home = home.clone();
                    if state.cwd.is_empty() {
                        home
                    } else {
                        state.cwd.clone()
                    }
                };
                self.stack.set_visible_child_name("files");
                self.toolbar.set_sensitive(true);
                self.navigate(&target);
                if let Some(handler) = self.on_connected.borrow().as_ref() {
                    handler();
                }
            }
        }
    }

    // --- навигация и содержимое

    fn navigate(&self, path: &str) {
        self.send(Command::Navigate {
            path: path.to_string(),
        });
    }

    /// Превращает введённый путь в абсолютный: `~`, относительные пути.
    fn resolve_input(&self, input: &str) -> String {
        let state = self.state.borrow();
        let input = input.trim();
        if input.is_empty() {
            state.cwd.clone()
        } else if input == "~" {
            state.home.clone()
        } else if let Some(rest) = input.strip_prefix("~/") {
            sftp::join_remote(&state.home, rest)
        } else if input.starts_with('/') {
            input.to_string()
        } else {
            sftp::join_remote(&state.cwd, input)
        }
    }

    fn show_dir(&self, path: String, entries: Vec<RemoteEntry>) {
        {
            let mut state = self.state.borrow_mut();
            state.cwd = path.clone();
            state.dirs.clear();
            state.dirs.insert(
                path.clone(),
                DirNode {
                    store: self.root_store.downgrade(),
                    entries,
                },
            );
        }
        self.root_store.remove_all();
        self.path_entry.set_text(&path);
        self.fill(&path, &self.root_store);
    }

    /// Узел дерева раскрыт: запоминаем его модель и запрашиваем содержимое.
    fn load_dir(&self, path: &str, store: &gio::ListStore) {
        self.state.borrow_mut().dirs.insert(
            path.to_string(),
            DirNode {
                store: store.downgrade(),
                entries: Vec::new(),
            },
        );
        self.send(Command::List {
            path: path.to_string(),
        });
    }

    fn on_listing(&self, path: &str, entries: Vec<RemoteEntry>) {
        let store = {
            let mut state = self.state.borrow_mut();
            let store = state.dirs.get(path).and_then(|node| node.store.upgrade());
            match (&store, state.dirs.get_mut(path)) {
                (Some(_), Some(node)) => node.entries = entries,
                // Узел уже свёрнут: ответ не нужен.
                _ => {
                    state.dirs.remove(path);
                }
            }
            store
        };
        if let Some(store) = store {
            self.fill(path, &store);
        }
    }

    fn fill(&self, path: &str, store: &gio::ListStore) {
        let visible = {
            let state = self.state.borrow();
            let Some(node) = state.dirs.get(path) else {
                return;
            };
            let mut visible: Vec<RemoteEntry> = node
                .entries
                .iter()
                .filter(|entry| state.show_hidden || !entry.name.starts_with('.'))
                .cloned()
                .collect();
            visible.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
            visible
        };
        sync_store(store, &visible);
    }

    /// Живые (показанные) каталоги; заодно забывает свёрнутые.
    fn live_dirs(&self) -> Vec<(String, gio::ListStore)> {
        let mut state = self.state.borrow_mut();
        state.dirs.retain(|_, node| node.store.upgrade().is_some());
        state
            .dirs
            .iter()
            .filter_map(|(path, node)| Some((path.clone(), node.store.upgrade()?)))
            .collect()
    }

    fn refill_all(&self) {
        for (path, store) in self.live_dirs() {
            self.fill(&path, &store);
        }
    }

    fn refresh_all(&self) {
        for (path, _) in self.live_dirs() {
            self.send(Command::List { path });
        }
    }

    fn refresh_dir(&self, path: &str) {
        let shown = self
            .state
            .borrow()
            .dirs
            .get(path)
            .is_some_and(|node| node.store.upgrade().is_some());
        if shown {
            self.send(Command::List {
                path: path.to_string(),
            });
        }
    }

    // --- выделение

    fn entry_at(&self, position: u32) -> Option<RemoteEntry> {
        let row = self
            .selection
            .item(position)
            .and_downcast::<gtk::TreeListRow>()?;
        let object = row.item().and_downcast::<glib::BoxedAnyObject>()?;
        let entry = object.borrow::<RemoteEntry>().clone();
        Some(entry)
    }

    fn selected(&self) -> Vec<RemoteEntry> {
        let bitset = self.selection.selection();
        let count = u32::try_from(bitset.size()).unwrap_or(u32::MAX);
        (0..count)
            .filter_map(|index| self.entry_at(bitset.nth(index)))
            .collect()
    }

    /// Выбранные файлы и каталоги, пригодные для передачи.
    fn selected_transferable(&self) -> Vec<RemoteEntry> {
        top_level(self.selected())
            .into_iter()
            .filter(|entry| entry.kind != EntryKind::Other)
            .collect()
    }

    fn popup_menu(&self, widget: &gtk::Widget, x: f64, y: f64) {
        let point = widget
            .compute_point(&self.view, &graphene::Point::new(x as f32, y as f32))
            .unwrap_or_else(|| graphene::Point::new(0.0, 0.0));
        let rect = gdk::Rectangle::new(point.x() as i32, point.y() as i32, 1, 1);
        self.context_menu.set_pointing_to(Some(&rect));
        self.context_menu.popup();
    }

    // --- действия

    fn activate_entry(self: &Rc<Self>, entry: RemoteEntry) {
        match entry.kind {
            EntryKind::Dir => self.navigate(&entry.path),
            EntryKind::File => self.open_for_edit(entry),
            EntryKind::Other => self
                .notifier
                .info(&tr!("“{}” cannot be opened: it is neither a file nor a folder", entry.name)),
        }
    }

    /// Путь для действий «в терминал» и «копировать»: выбранный элемент
    /// или, если ничего не выбрано, текущий каталог.
    fn target_path(&self) -> Option<RemoteEntry> {
        match self.selected().as_slice() {
            [entry] => Some(entry.clone()),
            _ => None,
        }
    }

    fn cd_terminal(&self) {
        let dir = match self.target_path() {
            Some(entry) if entry.kind == EntryKind::Dir => entry.path,
            Some(entry) => sftp::parent_remote(&entry.path),
            None => self.state.borrow().cwd.clone(),
        };
        if let Some(handler) = self.on_terminal_cd.borrow().as_ref() {
            handler(&dir);
        }
    }

    fn copy_path(&self) {
        let path = self
            .target_path()
            .map_or_else(|| self.state.borrow().cwd.clone(), |entry| entry.path);
        self.root.clipboard().set_text(&path);
        self.notifier.info(&tr!("Copied: {}", path));
    }

    fn open_selected(self: &Rc<Self>) {
        match self.selected().as_slice() {
            [entry] => self.activate_entry(entry.clone()),
            _ => self.notifier.info(tr!("Select a single file or folder")),
        }
    }

    fn make_dir(self: &Rc<Self>) {
        let cwd = self.state.borrow().cwd.clone();
        let weak = Rc::downgrade(self);
        dialogs::prompt_text(
            &self.root,
            tr!("New Folder"),
            &tr!("Name of the new folder in {}", cwd),
            "",
            tr!("Create"),
            move |name| {
                let Some(inner) = weak.upgrade() else { return };
                if name.contains('/') {
                    inner.notifier.error(tr!("The name cannot contain “/”"));
                    return;
                }
                inner.send(Command::Mkdir {
                    path: sftp::join_remote(&cwd, &name),
                });
            },
        );
    }

    fn rename_selected(self: &Rc<Self>) {
        let selected = self.selected();
        let [entry] = selected.as_slice() else {
            self.notifier.info(tr!("Select a single item to rename"));
            return;
        };
        let entry = entry.clone();
        let current_name = entry.name.clone();
        let weak = Rc::downgrade(self);
        dialogs::prompt_text(
            &self.root,
            tr!("Rename"),
            &tr!("New name for “{}”", current_name),
            &current_name,
            tr!("Rename"),
            move |name| {
                let Some(inner) = weak.upgrade() else { return };
                if name.contains('/') {
                    inner.notifier.error(tr!("The name cannot contain “/”"));
                    return;
                }
                if name == entry.name {
                    return;
                }
                let to = sftp::join_remote(&sftp::parent_remote(&entry.path), &name);
                inner.send(Command::Rename {
                    from: entry.path.clone(),
                    to,
                });
            },
        );
    }

    fn delete_selected(self: &Rc<Self>) {
        let entries = top_level(self.selected());
        if entries.is_empty() {
            self.notifier.info(tr!("Select what to delete"));
            return;
        }
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        let has_dirs = entries
            .iter()
            .any(|e| e.kind == EntryKind::Dir && !e.is_link);
        let message = if entries.len() == 1 {
            tr!("Delete “{}” from the Server?", names[0])
        } else {
            trn!(
                "Delete {} Item from the Server?",
                "Delete {} Items from the Server?",
                entries.len()
            )
        };
        let mut detail = summarize(&names);
        if has_dirs {
            detail.push('\n');
            detail.push_str(tr!("Folders are deleted with all their contents."));
        }
        detail.push('\n');
        detail.push_str(tr!("This action cannot be undone."));
        let paths: Vec<String> = entries.into_iter().map(|e| e.path).collect();
        let weak = Rc::downgrade(self);
        dialogs::confirm(
            &self.root,
            &message,
            &detail,
            tr!("Delete"),
            true,
            move || {
                if let Some(inner) = weak.upgrade() {
                    inner.send(Command::Delete { paths });
                }
            },
        );
    }

    // --- передачи

    fn download_selected(self: &Rc<Self>) {
        let entries = self.selected_transferable();
        if entries.is_empty() {
            self.notifier.info(tr!("Select files or folders to download"));
            return;
        }
        let dialog = gtk::FileDialog::builder()
            .title(tr!("Download to Folder"))
            .accept_label(tr!("Download Here"))
            .modal(true)
            .build();
        let weak = Rc::downgrade(self);
        dialog.select_folder(
            self.window().as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                let Some(inner) = weak.upgrade() else { return };
                match result {
                    Ok(folder) => match folder.path() {
                        Some(dest) => inner.download_to(entries, dest),
                        None => inner.notifier.error(tr!("The selected folder is not local")),
                    },
                    Err(err) if dialog_dismissed(&err) => {}
                    Err(err) => inner.notifier.error(&err.to_string()),
                }
            },
        );
    }

    fn download_to(self: &Rc<Self>, entries: Vec<RemoteEntry>, dest: PathBuf) {
        let conflicts: Vec<String> = entries
            .iter()
            .filter(|entry| dest.join(&entry.name).exists())
            .map(|entry| entry.name.clone())
            .collect();
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        let sources: Vec<String> = entries.into_iter().map(|e| e.path).collect();
        let weak = Rc::downgrade(self);
        let start = move || {
            if let Some(inner) = weak.upgrade() {
                inner.start_transfer(
                    &tr!("Downloading {}", summarize(&names)),
                    |id| Command::Download {
                        id,
                        sources,
                        dest_dir: dest,
                    },
                    None,
                    None,
                );
            }
        };
        if conflicts.is_empty() {
            start();
        } else {
            dialogs::confirm(
                &self.root,
                tr!("Replace Existing Local Files?"),
                &tr!("The selected folder already contains: {}", summarize(&conflicts)),
                tr!("Replace"),
                true,
                start,
            );
        }
    }

    fn choose_upload(self: &Rc<Self>, folder: bool) {
        let weak = Rc::downgrade(self);
        let window = self.window();
        if folder {
            let dialog = gtk::FileDialog::builder()
                .title(tr!("Upload Folder"))
                .accept_label(tr!("Upload"))
                .modal(true)
                .build();
            dialog.select_folder(window.as_ref(), gio::Cancellable::NONE, move |result| {
                let Some(inner) = weak.upgrade() else { return };
                match result {
                    Ok(file) => inner.upload_paths(file.path().into_iter().collect()),
                    Err(err) if dialog_dismissed(&err) => {}
                    Err(err) => inner.notifier.error(&err.to_string()),
                }
            });
        } else {
            let dialog = gtk::FileDialog::builder()
                .title(tr!("Upload Files"))
                .accept_label(tr!("Upload"))
                .modal(true)
                .build();
            dialog.open_multiple(window.as_ref(), gio::Cancellable::NONE, move |result| {
                let Some(inner) = weak.upgrade() else { return };
                match result {
                    Ok(files) => {
                        let paths = (0..files.n_items())
                            .filter_map(|i| files.item(i).and_downcast::<gio::File>())
                            .filter_map(|file| file.path())
                            .collect();
                        inner.upload_paths(paths);
                    }
                    Err(err) if dialog_dismissed(&err) => {}
                    Err(err) => inner.notifier.error(&err.to_string()),
                }
            });
        }
    }

    /// Загружает локальные файлы и каталоги в текущий удалённый каталог.
    fn upload_paths(self: &Rc<Self>, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let (cwd, existing) = {
            let state = self.state.borrow();
            if state.cwd.is_empty() {
                drop(state);
                self.notifier.error(tr!("No SFTP connection"));
                return;
            }
            let existing: Vec<String> = state
                .dirs
                .get(&state.cwd)
                .map(|node| node.entries.iter().map(|e| e.name.clone()).collect())
                .unwrap_or_default();
            (state.cwd.clone(), existing)
        };
        let names: Vec<String> = paths
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        let conflicts: Vec<String> = names
            .iter()
            .filter(|name| existing.contains(name))
            .cloned()
            .collect();

        let weak = Rc::downgrade(self);
        let dest = cwd.clone();
        let start = move || {
            if let Some(inner) = weak.upgrade() {
                inner.start_transfer(
                    &tr!("Uploading {}", summarize(&names)),
                    |id| Command::Upload {
                        id,
                        sources: paths,
                        dest_dir: dest.clone(),
                    },
                    Some(dest.clone()),
                    None,
                );
            }
        };
        if conflicts.is_empty() {
            start();
        } else {
            dialogs::confirm(
                &self.root,
                tr!("Replace Existing Files on the Server?"),
                &tr!("{} already contains: {}", cwd, summarize(&conflicts)),
                tr!("Replace"),
                true,
                start,
            );
        }
    }

    /// Создаёт строку прогресса и отправляет команду передачи.
    fn start_transfer(
        self: &Rc<Self>,
        title: &str,
        make_command: impl FnOnce(TransferId) -> Command,
        refresh: Option<String>,
        on_done: Option<DoneCallback>,
    ) {
        let id = NEXT_TRANSFER.fetch_add(1, Ordering::Relaxed);

        let label = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::Middle)
            .build();
        let cancel = gtk::Button::builder()
            .icon_name("process-stop-symbolic")
            .tooltip_text(tr!("Cancel transfer"))
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.append(&label);
        header.append(&cancel);
        let bar = gtk::ProgressBar::builder()
            .show_text(true)
            .text(tr!("Preparing…"))
            .margin_end(6)
            .build();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 2);
        widget.add_css_class("card");
        widget.add_css_class("sshdeck-transfer");
        widget.append(&header);
        widget.append(&bar);
        // Карточки завершённых передач, оставленные на время сворачивания.
        if !self.transfers_revealer.reveals_child() {
            while let Some(child) = self.transfers_box.first_child() {
                self.transfers_box.remove(&child);
            }
        }
        self.transfers_box.append(&widget);
        reveal(&self.transfers_revealer, &self.transfers_revealer, true, true);

        let weak = Rc::downgrade(self);
        cancel.connect_clicked(move |button| {
            if let Some(inner) = weak.upgrade() {
                button.set_sensitive(false);
                inner.send(Command::Cancel { id });
            }
        });

        self.state.borrow_mut().transfers.insert(
            id,
            TransferRow {
                widget,
                bar,
                title: title.to_string(),
                refresh,
                on_done,
            },
        );
        self.send(make_command(id));
    }

    fn on_progress(&self, id: TransferId, done: u64, total: u64, current: &str) {
        let state = self.state.borrow();
        let Some(row) = state.transfers.get(&id) else {
            return;
        };
        let fraction = if total == 0 {
            0.0
        } else {
            done as f64 / total as f64
        };
        row.bar.set_fraction(fraction.clamp(0.0, 1.0));
        let amounts = tr!("{} of {}", format_size(done), format_size(total));
        let text = if current.is_empty() {
            amounts
        } else {
            format!("{current} — {amounts}")
        };
        row.bar.set_text(Some(&text));
    }

    fn on_transfer_done(self: &Rc<Self>, id: TransferId, outcome: TransferOutcome) {
        let Some(row) = self.state.borrow_mut().transfers.remove(&id) else {
            return;
        };
        if self.state.borrow().transfers.is_empty() {
            // Последняя карточка уезжает вниз вместе с блоком.
            reveal(&self.transfers_revealer, &self.transfers_revealer, false, true);
        } else {
            self.transfers_box.remove(&row.widget);
        }
        match &outcome {
            TransferOutcome::Completed { skipped } => {
                if let Some(dir) = &row.refresh {
                    self.refresh_dir(dir);
                }
                if !skipped.is_empty() {
                    let names: Vec<String> = skipped
                        .iter()
                        .map(|path| sftp::base_name(path).to_string())
                        .collect();
                    self.notifier.info(&tr!(
                        "{} — skipped (links to folders and special files): {}",
                        row.title,
                        summarize(&names)
                    ));
                }
            }
            TransferOutcome::Cancelled => {}
            TransferOutcome::Failed(error) => {
                self.notifier.error(&format!("{}: {error}", row.title));
            }
        }
        if let Some(on_done) = row.on_done {
            on_done(&outcome);
        }
    }

    // --- временные файлы

    fn new_scratch_dir(&self, kind: &str) -> Result<PathBuf, String> {
        let seq = {
            let mut state = self.state.borrow_mut();
            state.scratch_seq += 1;
            state.scratch_seq
        };
        let dir = self.scratch_dir.join(format!("{kind}-{seq}"));
        session::ensure_private_dir(&self.scratch_dir)
            .and_then(|()| session::ensure_private_dir(&dir))
            .map_err(|err| tr!("Could not create a temporary folder: {}", err))?;
        Ok(dir)
    }

    // --- перетаскивание наружу

    fn drag_content(self: &Rc<Self>) -> Option<gdk::ContentProvider> {
        let entries = self.selected_transferable();
        if entries.is_empty() {
            return None;
        }
        let weak = Rc::downgrade(self);
        let provider = RemoteFilesProvider::new(Box::new(move || {
            let weak = weak.clone();
            let entries = entries.clone();
            Box::pin(async move {
                let inner = weak
                    .upgrade()
                    .ok_or_else(|| tr!("the tab is closed").to_string())?;
                let dir = inner.new_scratch_dir("drag")?;
                let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
                let sources: Vec<String> = entries.into_iter().map(|e| e.path).collect();
                let (tx, rx) = async_channel::bounded(1);
                inner.start_transfer(
                    &tr!("Dragging {}", summarize(&names)),
                    |id| Command::Download {
                        id,
                        sources,
                        dest_dir: dir.clone(),
                    },
                    None,
                    Some(Box::new(move |outcome| {
                        let _ = tx.try_send(outcome.clone());
                    })),
                );
                drop(inner);
                match rx.recv().await {
                    Ok(TransferOutcome::Completed { .. }) => {
                        Ok(names.iter().map(|name| dir.join(name)).collect())
                    }
                    Ok(TransferOutcome::Cancelled) => Err(tr!("the transfer was cancelled").to_string()),
                    Ok(TransferOutcome::Failed(error)) => Err(error),
                    Err(_) => Err(tr!("the transfer was interrupted").to_string()),
                }
            })
        }));
        Some(provider.upcast())
    }

    // --- правка в локальном редакторе

    fn open_for_edit(self: &Rc<Self>, entry: RemoteEntry) {
        // Файл уже открыт: повторное скачивание затёрло бы несохранённую
        // на сервере правку, поэтому просто открываем копию ещё раз.
        let existing = self
            .state
            .borrow()
            .edits
            .iter()
            .find(|watch| watch.remote == entry.path)
            .map(|watch| watch.local.clone());
        if let Some(local) = existing {
            self.open_local(&local);
            return;
        }

        if entry.size.unwrap_or(0) > LARGE_EDIT_BYTES {
            let weak = Rc::downgrade(self);
            let size = format_size(entry.size.unwrap_or(0));
            dialogs::confirm(
                &self.root,
                &tr!("Open “{}”?", entry.name),
                &tr!(
                    "The file is large ({}); it will be downloaded in full to a temporary folder.",
                    size
                ),
                tr!("Open"),
                false,
                move || {
                    if let Some(inner) = weak.upgrade() {
                        inner.begin_edit(entry);
                    }
                },
            );
        } else {
            self.begin_edit(entry);
        }
    }

    fn begin_edit(self: &Rc<Self>, entry: RemoteEntry) {
        let dir = match self.new_scratch_dir("edit") {
            Ok(dir) => dir,
            Err(message) => {
                self.notifier.error(&message);
                return;
            }
        };
        let local = dir.join(&entry.name);
        let remote = entry.path.clone();
        let weak = Rc::downgrade(self);
        self.start_transfer(
            &tr!("Opening {}", entry.name),
            |id| Command::Download {
                id,
                sources: vec![entry.path],
                dest_dir: dir,
            },
            None,
            Some(Box::new(move |outcome| {
                if let (Some(inner), TransferOutcome::Completed { .. }) = (weak.upgrade(), outcome)
                {
                    inner.watch_and_open(remote, local);
                }
            })),
        );
    }

    fn watch_and_open(self: &Rc<Self>, remote: String, local: PathBuf) {
        // Наблюдаем за каталогом, а не за файлом: многие редакторы сохраняют
        // через временный файл и переименование.
        let monitor = local.parent().and_then(|dir| {
            gio::File::for_path(dir)
                .monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
                .map_err(|err| {
                    self.notifier.error(&tr!(
                        "Could not watch “{}” for changes: {}",
                        local.display(),
                        err
                    ));
                })
                .ok()
        });
        let watch = Rc::new(EditWatch {
            remote,
            stamp: Cell::new(file_stamp(&local)),
            local,
            monitor,
            uploading: Cell::new(false),
            dirty: Cell::new(false),
            generation: Cell::new(0),
        });
        if let Some(monitor) = &watch.monitor {
            let weak = Rc::downgrade(self);
            let weak_watch = Rc::downgrade(&watch);
            monitor.connect_changed(move |_, _, _, _| {
                if let (Some(inner), Some(watch)) = (weak.upgrade(), weak_watch.upgrade()) {
                    inner.schedule_edit_check(&watch);
                }
            });
        }
        self.open_local(&watch.local);
        self.state.borrow_mut().edits.push(watch);
    }

    fn schedule_edit_check(self: &Rc<Self>, watch: &Rc<EditWatch>) {
        let generation = watch.generation.get().wrapping_add(1);
        watch.generation.set(generation);
        let weak = Rc::downgrade(self);
        let weak_watch = Rc::downgrade(watch);
        glib::timeout_add_local_once(EDIT_DEBOUNCE, move || {
            if let (Some(inner), Some(watch)) = (weak.upgrade(), weak_watch.upgrade())
                && watch.generation.get() == generation
            {
                inner.upload_if_changed(&watch);
            }
        });
    }

    fn upload_if_changed(self: &Rc<Self>, watch: &Rc<EditWatch>) {
        if watch.uploading.get() {
            watch.dirty.set(true);
            return;
        }
        // Файла может не быть в момент пересохранения — дождёмся следующего
        // события.
        let Some(stamp) = file_stamp(&watch.local) else {
            return;
        };
        if watch.stamp.get() == Some(stamp) {
            return;
        }
        watch.stamp.set(Some(stamp));
        watch.uploading.set(true);

        let name = sftp::base_name(&watch.remote).to_string();
        let weak = Rc::downgrade(self);
        let weak_watch = Rc::downgrade(watch);
        let (local, remote) = (watch.local.clone(), watch.remote.clone());
        self.start_transfer(
            &tr!("Saving {} to the server", name),
            |id| Command::UploadFile { id, local, remote },
            Some(sftp::parent_remote(&watch.remote)),
            Some(Box::new(move |outcome| {
                let (Some(inner), Some(watch)) = (weak.upgrade(), weak_watch.upgrade()) else {
                    return;
                };
                watch.uploading.set(false);
                match outcome {
                    TransferOutcome::Completed { .. } => inner
                        .notifier
                        .info(&tr!("Saved to the server: {}", sftp::base_name(&watch.remote))),
                    // Не удалось: при следующем сохранении попробуем снова.
                    _ => watch.stamp.set(None),
                }
                if watch.dirty.replace(false) {
                    inner.upload_if_changed(&watch);
                }
            })),
        );
    }

    fn open_local(&self, path: &Path) {
        let spawned = std::process::Command::new("xdg-open")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(err) => self
                .notifier
                .error(&tr!("Could not start {}: {}", "xdg-open", err)),
        }
    }
}
