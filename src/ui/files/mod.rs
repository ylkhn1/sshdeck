//! Панель файлов: дерево удалённого каталога, по желанию — дерево этого
//! компьютера над ним, операции с файлами, передачи с прогрессом, правка
//! файлов в локальном редакторе.
//!
//! Панель ничего не делает с сетью сама: она отправляет команды фоновой
//! SFTP-задаче и обновляет виджеты по её событиям. Само дерево файлов
//! ([`tree::FileTree`]) от SFTP не зависит: каталоги оно просит у панели,
//! а та читает их по SFTP или с локального диска ([`local`]).

mod drag;
mod edit;
mod local;
mod transfers;
mod tree;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::{Notifier, collapsing_revealer, dialogs, parent_window};
use crate::session::Session;
use crate::sftp::{self, Command, ConnState, EntryKind, Event, FileEntry, SftpWorker, TransferId};
use edit::EditWatch;
use local::LocalSide;
use transfers::{TransferRow, summarize};
use tree::{FileTree, Side, TreeRequest, top_level};

/// Номера панелей: по нему перетаскивание узнаёт свою панель.
static NEXT_PANEL: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct FilePanel {
    inner: Rc<Inner>,
}

/// Просьба терминалу перейти в каталог.
type CdHandler = Box<dyn Fn(&Path)>;
/// Пользователь включил или выключил дерево этого компьютера.
type ToggleHandler = Box<dyn Fn(bool)>;

struct Inner {
    /// Номер панели (уникален в процессе).
    id: u64,
    /// Адрес вкладки (`Session::target`): по нему помнится последняя
    /// локальная папка.
    address: String,
    root: gtk::Box,
    /// Шапка: строка навигации сервера (пока локального дерева нет) и
    /// кнопки действий.
    header: gtk::Box,
    /// Место строки навигации сервера над его деревом, когда показано и
    /// локальное дерево.
    remote_nav_box: gtk::Box,
    /// Дерево этого компьютера сверху, дерево сервера снизу.
    paned: gtk::Paned,
    remote_section: gtk::Box,
    /// Дерево файлов сервера.
    remote: FileTree,
    /// Дерево этого компьютера; создаётся при первом показе.
    local: OnceCell<LocalSide>,
    local_toggle: gtk::ToggleButton,
    /// Переключатель меняется по просьбе окна (синхронизация вкладок), а
    /// не пользователем: окну об этом не сообщается.
    local_syncing: Cell<bool>,
    on_local_toggled: RefCell<Option<ToggleHandler>>,
    /// Дерево, с которым работали последним: от него считается
    /// направление копирования.
    active_side: Cell<Side>,
    /// SFTP-соединение установлено (дерево сервера показано).
    connected: Cell<bool>,
    /// Кнопки, которым нужно SFTP-соединение.
    remote_ops: Vec<gtk::Widget>,
    /// «Загрузить» и «Скачать» с подписями (одно дерево) и только
    /// значками (два дерева).
    labeled_ops: [gtk::Widget; 2],
    compact_ops: [gtk::Widget; 2],
    mkdir_button: gtk::Button,
    copy_button: gtk::Button,
    /// «Скачать в локальную папку» в меню дерева сервера.
    copy_action: gio::SimpleAction,
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
    /// Все терминалы вкладки ждут входа, который идёт в другой вкладке.
    login_elsewhere: Cell<bool>,
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
    /// Последнее состояние соединения, о котором сообщила SFTP-задача.
    last_conn: Option<ConnState>,
    /// Загрузки в каталоги, содержимое которых ещё не прочитано: ждут
    /// списка, чтобы спросить о замене существующих файлов.
    pending_uploads: Vec<(PathBuf, Vec<PathBuf>)>,
    transfers: HashMap<TransferId, TransferRow>,
    edits: Vec<Rc<EditWatch>>,
    scratch_seq: u32,
}

impl FilePanel {
    /// `show_local` — показать ли сразу дерево этого компьютера.
    pub fn new(
        session: &Session,
        scratch_dir: PathBuf,
        notifier: Notifier,
        show_local: bool,
    ) -> Self {
        let inner = Rc::new_cyclic(|weak| {
            Inner::build(weak, session.target().to_string(), scratch_dir, notifier)
        });
        inner.sync_header();

        let (tx, rx) = async_channel::unbounded();
        match SftpWorker::start(session.clone(), tx) {
            Ok(worker) => *inner.worker.borrow_mut() = Some(worker),
            Err(err) => inner.show_status(false, tr!("Could Not Start SFTP"), &err.to_string()),
        }
        let weak = Rc::downgrade(&inner);
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                inner.handle_event(event);
            }
        });
        let panel = Self { inner };
        panel.set_local_files_visible(show_local);
        panel
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.inner.root
    }

    /// Показывает или прячет дерево этого компьютера (по просьбе окна —
    /// обработчик переключения при этом не вызывается).
    pub fn set_local_files_visible(&self, visible: bool) {
        let inner = &self.inner;
        inner.local_syncing.set(true);
        inner.local_toggle.set_active(visible);
        inner.local_syncing.set(false);
    }

    /// Пользователь включил или выключил дерево этого компьютера.
    pub fn set_local_toggle_handler(&self, handler: impl Fn(bool) + 'static) {
        *self.inner.on_local_toggled.borrow_mut() = Some(Box::new(handler));
    }

    /// Сообщает панели, работает ли ssh в терминале вкладки.
    pub fn set_ssh_running(&self, running: bool) {
        self.inner.set_ssh_running(running);
    }

    /// Сообщает панели, что терминалы вкладки ждут входа в другой вкладке:
    /// тогда просьба ввести пароль «в терминале справа» была бы неверной.
    pub fn set_login_elsewhere(&self, elsewhere: bool) {
        let inner = &self.inner;
        if inner.login_elsewhere.replace(elsewhere) == elsewhere {
            return;
        }
        let last = inner.state.borrow().last_conn.clone();
        if let Some(state @ ConnState::Waiting { last_error: None }) = last {
            inner.on_state(state);
        }
    }

    /// Терминал сообщил свой текущий каталог: панель переходит туда же,
    /// если слежение включено и соединение установлено. Путь может быть
    /// и не в UTF-8 (`Path` из байтов).
    pub fn follow_dir(&self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        let inner = &self.inner;
        let ready = matches!(
            inner.state.borrow().last_conn,
            Some(ConnState::Connected { .. })
        ) && inner.remote.cwd() != path;
        if ready && inner.follow.get() && path.has_root() {
            inner.remote.navigate(path);
        }
    }

    pub fn set_connected_handler(&self, handler: impl Fn() + 'static) {
        *self.inner.on_connected.borrow_mut() = Some(Box::new(handler));
    }

    pub fn set_terminal_cd_handler(&self, handler: impl Fn(&Path) + 'static) {
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
        inner.remote.shutdown();
        if let Some(local) = inner.local.get() {
            local.tree.shutdown();
        }
        if inner.scratch_dir.exists() {
            let _ = std::fs::remove_dir_all(&inner.scratch_dir);
        }
    }
}

// ------------------------------------------------------------ построение

impl Inner {
    fn build(
        weak: &Weak<Inner>,
        address: String,
        scratch_dir: PathBuf,
        notifier: Notifier,
    ) -> Inner {
        // --- дерево файлов сервера и его контекстное меню
        let menu = gio::Menu::new();
        let open_section = gio::Menu::new();
        open_section.append(Some(tr!("Open or Edit")), Some("files.open"));
        let copy_item = gio::MenuItem::new(
            Some(tr!("Download to Local Folder")),
            Some("files.copy-other"),
        );
        // Пункт есть, только пока показано дерево этого компьютера.
        copy_item.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
        open_section.append_item(&copy_item);
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
        let remote = FileTree::new(Side::Remote, menu.upcast_ref());

        // --- панель инструментов
        let local_toggle = gtk::ToggleButton::builder()
            .icon_name("computer-symbolic")
            .tooltip_text(tr!("Show files on this computer"))
            .build();
        let upload_menu = gio::Menu::new();
        upload_menu.append(Some(tr!("Files…")), Some("files.upload-files"));
        upload_menu.append(Some(tr!("Folder…")), Some("files.upload-folder"));
        // «Загрузить» и «Скачать» в двух видах: с подписями и (при двух
        // деревьях, чтобы панель не становилась шире) только значками.
        let transfer_buttons = |labeled: bool| {
            let upload = gtk::MenuButton::builder()
                .always_show_arrow(true)
                .tooltip_text(tr!("Upload to the current folder on the server"))
                .menu_model(&upload_menu)
                .visible(labeled)
                .build();
            let download = gtk::Button::builder()
                .tooltip_text(tr!("Download the selection to a local folder"))
                .action_name("files.download")
                .visible(labeled)
                .build();
            if labeled {
                upload.set_child(Some(
                    &adw::ButtonContent::builder()
                        .icon_name("document-send-symbolic")
                        .label(tr!("Upload"))
                        .build(),
                ));
                download.set_child(Some(
                    &adw::ButtonContent::builder()
                        .icon_name("folder-download-symbolic")
                        .label(tr!("Download"))
                        .build(),
                ));
            } else {
                upload.set_icon_name("document-send-symbolic");
                download.set_icon_name("folder-download-symbolic");
            }
            [upload.upcast::<gtk::Widget>(), download.upcast()]
        };
        let labeled_ops = transfer_buttons(true);
        let compact_ops = transfer_buttons(false);
        // Не забирает фокус: F5 после щелчка продолжает работать в дереве.
        let copy_button = gtk::Button::builder()
            .focus_on_click(false)
            .visible(false)
            .build();
        let mkdir_button = gtk::Button::builder()
            .icon_name("folder-new-symbolic")
            .tooltip_text(tr!("New folder"))
            .build();
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
        ops_row.append(&local_toggle);
        for button in labeled_ops.iter().chain(&compact_ops) {
            ops_row.append(button);
        }
        ops_row.append(&spacer);
        ops_row.append(&copy_button);
        ops_row.append(&mkdir_button);
        ops_row.append(&follow_toggle);
        ops_row.append(&hidden_toggle);

        // Пока локального дерева нет, всё как раньше: строка навигации
        // сервера над кнопками действий.
        let header = gtk::Box::new(gtk::Orientation::Vertical, 0);
        header.add_css_class("sshdeck-files-toolbar");
        header.append(remote.nav_row());
        header.append(&ops_row);

        // --- деревья
        let remote_nav_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        remote_nav_box.add_css_class("sshdeck-files-toolbar");
        remote_nav_box.set_visible(false);
        let remote_section = gtk::Box::new(gtk::Orientation::Vertical, 0);
        remote_section.add_css_class("sshdeck-files-section");
        remote_section.append(&remote_nav_box);
        remote_section.append(remote.content());
        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Vertical)
            .end_child(&remote_section)
            .resize_start_child(true)
            .resize_end_child(true)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .vexpand(true)
            .css_classes(["sshdeck-files-split"])
            .build();

        // Показывается, только пока есть активные передачи.
        let transfers_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        transfers_box.add_css_class("sshdeck-transfers");
        let transfers_revealer =
            collapsing_revealer(gtk::RevealerTransitionType::SlideUp, &transfers_box);
        transfers_revealer.set_visible(false);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_width_request(300);
        root.append(&header);
        root.append(&paned);
        root.append(&transfers_revealer);

        // --- действия
        let actions = gio::SimpleActionGroup::new();
        add_action(&actions, "refresh", weak, |inner| {
            inner.remote.refresh_all()
        });
        add_action(&actions, "open", weak, |inner| inner.open_selected());
        add_action(&actions, "download", weak, |inner| {
            inner.download_selected()
        });
        add_action(&actions, "upload-files", weak, |inner| {
            inner.choose_upload(false)
        });
        add_action(&actions, "upload-folder", weak, |inner| {
            inner.choose_upload(true)
        });
        add_action(&actions, "mkdir", weak, |inner| inner.make_dir());
        add_action(&actions, "rename", weak, |inner| inner.rename_selected());
        add_action(&actions, "delete", weak, |inner| inner.delete_selected());
        add_action(&actions, "cd-terminal", weak, |inner| inner.cd_terminal());
        add_action(&actions, "copy-path", weak, |inner| inner.copy_path());
        let copy_action = add_action(&actions, "copy-other", weak, |inner| {
            inner.copy_to_other(Side::Remote)
        });
        copy_action.set_enabled(false);
        root.insert_action_group("files", Some(&actions));
        add_tree_shortcuts(
            remote.view(),
            weak,
            Side::Remote,
            "files.rename",
            "files.delete",
        );

        // --- обработчики
        connect(weak, |weak| {
            remote.set_request_handler(move |side, request| {
                if let Some(inner) = weak.upgrade() {
                    inner.tree_request(side, request);
                }
            });
        });
        connect(weak, |weak| {
            remote.set_activate_handler(move |entry| {
                if let Some(inner) = weak.upgrade() {
                    inner.activate_entry(entry);
                }
            });
        });
        connect(weak, |weak| {
            remote.set_drag_handler(move || weak.upgrade()?.drag_content(Side::Remote));
        });
        connect(weak, |weak| {
            local_toggle.connect_toggled(move |toggle| {
                if let Some(inner) = weak.upgrade() {
                    inner.on_local_toggled(toggle.is_active());
                }
            });
        });
        connect(weak, |weak| {
            copy_button.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.copy_to_other(inner.active_side.get());
                }
            });
        });
        connect(weak, |weak| {
            mkdir_button.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    match inner.shown_active_side() {
                        Side::Local => inner.make_local_dir(),
                        Side::Remote => inner.make_dir(),
                    }
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
                    inner.remote.set_show_hidden(toggle.is_active());
                }
            });
        });
        track_active_side(&remote_section, weak, Side::Remote);
        attach_drop_target(remote.content(), weak, Side::Remote);

        let mut remote_ops: Vec<gtk::Widget> =
            labeled_ops.iter().chain(&compact_ops).cloned().collect();
        remote_ops.extend([follow_toggle.upcast(), hidden_toggle.upcast()]);
        Inner {
            id: NEXT_PANEL.fetch_add(1, Ordering::Relaxed),
            address,
            root,
            header,
            remote_nav_box,
            paned,
            remote_section,
            remote,
            local: OnceCell::new(),
            local_toggle,
            local_syncing: Cell::new(false),
            on_local_toggled: RefCell::new(None),
            active_side: Cell::new(Side::Remote),
            connected: Cell::new(false),
            remote_ops,
            labeled_ops,
            compact_ops,
            mkdir_button,
            copy_button,
            copy_action,
            transfers_box,
            transfers_revealer,
            notifier,
            scratch_dir,
            worker: RefCell::new(None),
            ssh_running: Cell::new(true),
            login_elsewhere: Cell::new(false),
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

fn add_action(
    group: &gio::SimpleActionGroup,
    name: &str,
    weak: &Weak<Inner>,
    handler: impl Fn(&Rc<Inner>) + 'static,
) -> gio::SimpleAction {
    let action = gio::SimpleAction::new(name, None);
    let weak = weak.clone();
    action.connect_activate(move |_, _| {
        if let Some(inner) = weak.upgrade() {
            handler(&inner);
        }
    });
    group.add_action(&action);
    action
}

/// Горячие клавиши таблицы дерева: F5 — копировать на другую сторону
/// (если показаны оба дерева) или обновить, Ctrl+R — обновить, F2 и
/// Delete — действия `rename` и `delete` своей стороны.
fn add_tree_shortcuts(
    view: &gtk::ColumnView,
    weak: &Weak<Inner>,
    side: Side,
    rename: &str,
    delete: &str,
) {
    let shortcuts = gtk::ShortcutController::new();
    let callback = |handler: fn(&Rc<Inner>, Side)| {
        let weak = weak.clone();
        gtk::CallbackAction::new(move |_, _| {
            if let Some(inner) = weak.upgrade() {
                handler(&inner, side);
            }
            glib::Propagation::Stop
        })
    };
    shortcuts.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("F5"),
        Some(callback(|inner, side| {
            if inner.local_visible() {
                inner.copy_to_other(side);
            } else {
                inner.refresh_tree(side);
            }
        })),
    ));
    shortcuts.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Control>r"),
        Some(callback(Inner::refresh_tree)),
    ));
    for (trigger, action) in [("F2", rename), ("Delete", delete)] {
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(trigger),
            Some(gtk::NamedAction::new(action)),
        ));
    }
    view.add_controller(shortcuts);
}

/// Отмечает дерево активным, когда в него переходит фокус или по нему
/// щёлкают.
fn track_active_side(section: &gtk::Box, weak: &Weak<Inner>, side: Side) {
    let focus = gtk::EventControllerFocus::new();
    {
        let weak = weak.clone();
        focus.connect_enter(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.set_active_side(side);
            }
        });
    }
    section.add_controller(focus);
    // Щелчок не всегда переносит фокус (пустое место, полоса прокрутки);
    // жест в фазе захвата событие не забирает.
    let click = gtk::GestureClick::builder()
        .button(0)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    let weak = weak.clone();
    click.connect_pressed(move |_, _, _, _| {
        if let Some(inner) = weak.upgrade() {
            inner.set_active_side(side);
        }
    });
    section.add_controller(click);
}

/// Приём перетаскивания на дерево стороны `side`: на строку каталога — в
/// этот каталог, иначе — в текущий. Что принимать, решает
/// [`drag::drop_action`].
fn attach_drop_target(content: &gtk::Stack, weak: &Weak<Inner>, side: Side) {
    let drop = gtk::DropTarget::new(glib::Type::INVALID, gdk::DragAction::COPY);
    drop.set_types(&[drag::FileDrag::static_type(), gdk::FileList::static_type()]);
    {
        let weak = weak.clone();
        drop.connect_motion(move |target, x, y| {
            let Some(inner) = weak.upgrade() else {
                return gdk::DragAction::empty();
            };
            let tree = inner.tree(side);
            if inner.drop_transfer(target, side).is_none() {
                tree.highlight_drop_target(None, false);
                return gdk::DragAction::empty();
            }
            let row = tree
                .drop_row_at(x, y)
                .filter(|(_, entry)| entry.kind == EntryKind::Dir)
                .map(|(row, _)| row);
            tree.highlight_drop_target(row.as_ref(), true);
            gdk::DragAction::COPY
        });
    }
    {
        let weak = weak.clone();
        drop.connect_leave(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.tree(side).highlight_drop_target(None, false);
            }
        });
    }
    let weak = weak.clone();
    drop.connect_drop(move |target, value, x, y| {
        let Some(inner) = weak.upgrade() else {
            return false;
        };
        let tree = inner.tree(side);
        tree.highlight_drop_target(None, false);
        if inner.drop_transfer(target, side).is_none() {
            return false;
        }
        let dest = tree
            .drop_row_at(x, y)
            .filter(|(_, entry)| entry.kind == EntryKind::Dir)
            .map(|(_, entry)| entry.path);
        inner.accept_drop(side, value, dest)
    });
    content.add_controller(drop);
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

    /// Дерево стороны. Локальное запрашивается, только когда оно уже есть.
    fn tree(&self, side: Side) -> &FileTree {
        match (side, self.local.get()) {
            (Side::Local, Some(local)) => &local.tree,
            _ => &self.remote,
        }
    }

    /// Показано ли дерево этого компьютера.
    fn local_visible(&self) -> bool {
        self.local_toggle.is_active() && self.local.get().is_some()
    }

    /// Активное дерево; без локального дерева — всегда сервер.
    fn shown_active_side(&self) -> Side {
        if self.local_visible() {
            self.active_side.get()
        } else {
            Side::Remote
        }
    }

    /// Экран состояния вместо дерева сервера; `busy` включает индикатор
    /// ожидания. Локальное дерево и его переключатель остаются доступны.
    fn show_status(&self, busy: bool, title: &str, description: &str) {
        self.remote.show_status(busy, title, description);
        self.connected.set(false);
        self.sync_header();
    }

    /// Доступность и вид кнопок шапки: что требует SFTP, что зависит от
    /// активного дерева.
    fn sync_header(&self) {
        let connected = self.connected.get();
        for widget in &self.remote_ops {
            widget.set_sensitive(connected);
        }
        let local_shown = self.local_visible();
        let side = self.shown_active_side();
        self.mkdir_button
            .set_sensitive(connected || side == Side::Local);
        self.copy_button.set_visible(local_shown);
        self.copy_button.set_sensitive(connected);
        let (icon, tooltip) = match side {
            // Локальное дерево сверху, серверное снизу: стрелка показывает,
            // куда пойдут файлы.
            Side::Local => (
                "go-down-symbolic",
                tr!("Upload the selection to the server folder below (F5)"),
            ),
            Side::Remote => (
                "go-up-symbolic",
                tr!("Download the selection to the local folder above (F5)"),
            ),
        };
        self.copy_button.set_icon_name(icon);
        self.copy_button.set_tooltip_text(Some(tooltip));
        self.mkdir_button.set_tooltip_text(Some(match side {
            Side::Local => tr!("New folder on this computer"),
            Side::Remote => tr!("New folder"),
        }));
    }

    /// Пользователь (или окно) переключил дерево этого компьютера.
    fn on_local_toggled(self: &Rc<Self>, visible: bool) {
        if visible {
            let local = self.ensure_local();
            local.section.set_visible(true);
        } else if let Some(local) = self.local.get() {
            local.section.set_visible(false);
        }
        // Строка навигации сервера: в шапке (как без локального дерева)
        // или над деревом сервера.
        let nav = self.remote.nav_row();
        if let Some(parent) = nav.parent().and_downcast::<gtk::Box>() {
            parent.remove(nav);
        }
        if visible {
            self.remote_nav_box.append(nav);
            self.paned.add_css_class("split");
        } else {
            self.header.prepend(nav);
            self.paned.remove_css_class("split");
            self.active_side.set(Side::Remote);
        }
        self.remote_nav_box.set_visible(visible);
        for button in &self.labeled_ops {
            button.set_visible(!visible);
        }
        for button in &self.compact_ops {
            button.set_visible(visible);
        }
        self.remote.set_location_icon(
            visible.then_some(("network-server-symbolic", tr!("Files on the server"))),
        );
        self.copy_action.set_enabled(visible);
        self.set_active_side(self.active_side.get());
        if !self.local_syncing.get()
            && let Some(handler) = self.on_local_toggled.borrow().as_ref()
        {
            handler(visible);
        }
    }

    fn set_active_side(&self, side: Side) {
        self.active_side.set(side);
        let local_shown = self.local_visible();
        let sections = [
            (Side::Remote, Some(&self.remote_section)),
            (Side::Local, self.local.get().map(|local| &local.section)),
        ];
        for (section_side, section) in sections {
            if let Some(section) = section {
                if local_shown && section_side == side {
                    section.add_css_class("active");
                } else {
                    section.remove_css_class("active");
                }
            }
        }
        self.sync_header();
    }

    fn refresh_tree(self: &Rc<Self>, side: Side) {
        self.tree(side).refresh_all();
    }

    /// Перечитывает каталог дерева стороны `side`, если он показан.
    fn refresh_side(&self, side: Side, dir: &Path) {
        match side {
            Side::Remote => self.remote.refresh_dir(dir),
            Side::Local => {
                if let Some(local) = self.local.get() {
                    local.tree.refresh_dir(dir);
                }
            }
        }
    }

    /// Запрос дерева: каталоги сервера читает SFTP-задача, локальные —
    /// фоновые потоки.
    fn tree_request(self: &Rc<Self>, side: Side, request: TreeRequest) {
        match (side, request) {
            (Side::Remote, TreeRequest::Navigate(path)) => self.send(Command::Navigate { path }),
            (Side::Remote, TreeRequest::List(path)) => self.send(Command::List { path }),
            (Side::Local, request) => self.local_request(request),
        }
    }

    fn handle_event(self: &Rc<Self>, event: Event) {
        match event {
            Event::State(state) => self.on_state(state),
            Event::Navigated { requested, result } => match result {
                Ok((path, entries)) => self.remote.show_dir(path, entries),
                Err(err) => {
                    self.notifier.error(&tr!(
                        "Could not open “{}”: {}",
                        sftp::display(&requested),
                        err
                    ));
                    self.remote.navigate_failed();
                }
            },
            Event::Listing { path, result } => match result {
                Ok(entries) => {
                    self.start_pending_uploads(&path, &entries);
                    self.remote.on_listing(&path, entries);
                }
                Err(err) => {
                    self.state
                        .borrow_mut()
                        .pending_uploads
                        .retain(|(dir, _)| *dir != path);
                    self.notifier
                        .error(&tr!("Could not read “{}”: {}", sftp::display(&path), err));
                }
            },
            Event::OpDone { error, refresh } => {
                if let Some(error) = error {
                    self.notifier.error(&error);
                }
                for dir in refresh {
                    self.remote.refresh_dir(&dir);
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
            ConnState::Waiting { last_error: None } if self.login_elsewhere.get() => self
                .show_status(
                    true,
                    tr!("Waiting for Login"),
                    tr!(
                        "Someone is logging in to this server in another tab — the file panel will \
                         connect automatically."
                    ),
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
                &tr!(
                    "{}\nThe panel will connect once you log in in the terminal.",
                    error
                ),
            ),
            ConnState::Connecting => self.show_status(true, tr!("Connecting…"), ""),
            ConnState::Connected { home } => {
                self.remote.set_home(home.clone());
                let cwd = self.remote.cwd();
                let target = if cwd.as_os_str().is_empty() {
                    home
                } else {
                    cwd
                };
                self.remote.show_files();
                self.connected.set(true);
                self.sync_header();
                self.remote.navigate(&target);
                if let Some(handler) = self.on_connected.borrow().as_ref() {
                    handler();
                }
            }
        }
    }

    // --- копирование на другую сторону

    /// Копирует выбранное в дереве `from` в текущий каталог другого дерева.
    fn copy_to_other(self: &Rc<Self>, from: Side) {
        let Some(local) = self.local.get().filter(|_| self.local_visible()) else {
            return;
        };
        let (selected, dest) = match from {
            Side::Local => (local.tree.selected(), self.remote.cwd()),
            Side::Remote => (self.remote.selected(), local.tree.cwd()),
        };
        match copy_plan(from, selected, &dest, self.connected.get()) {
            CopyPlan::Upload { paths, dest } => self.upload_paths(paths, Some(dest)),
            CopyPlan::Download { entries, dest } => self.download_to(entries, dest),
            CopyPlan::NothingSelected => self.notifier.info(match from {
                Side::Local => tr!("Select files or folders to upload"),
                Side::Remote => tr!("Select files or folders to download"),
            }),
            CopyPlan::NotConnected => self.notifier.error(tr!("No SFTP connection")),
            CopyPlan::NoDestination => self
                .notifier
                .error(tr!("There is no open folder to copy to")),
        }
    }

    // --- действия

    fn activate_entry(self: &Rc<Self>, entry: FileEntry) {
        match entry.kind {
            EntryKind::Dir => self.remote.navigate(&entry.path),
            EntryKind::File => self.open_for_edit(entry),
            EntryKind::Other => self.notifier.info(&tr!(
                "“{}” cannot be opened: it is neither a file nor a folder",
                entry.display_name()
            )),
        }
    }

    /// Путь для действий «в терминал» и «копировать»: выбранный элемент
    /// или, если ничего не выбрано, текущий каталог.
    fn target_path(&self) -> Option<FileEntry> {
        match self.remote.selected().as_slice() {
            [entry] => Some(entry.clone()),
            _ => None,
        }
    }

    fn cd_terminal(&self) {
        let dir = match self.target_path() {
            Some(entry) if entry.kind == EntryKind::Dir => entry.path,
            Some(entry) => sftp::parent_remote(&entry.path),
            None => self.remote.cwd(),
        };
        // Путь передаётся байтами: команда cd справится и с именами не из
        // UTF-8 (см. `remote::cd_command`).
        if let Some(handler) = self.on_terminal_cd.borrow().as_ref() {
            handler(&dir);
        }
    }

    fn copy_path(&self) {
        let path = self
            .target_path()
            .map_or_else(|| self.remote.cwd(), |entry| entry.path);
        let text = sftp::display(&path);
        self.root.clipboard().set_text(&text);
        self.notifier.info(&tr!("Copied: {}", text));
    }

    fn open_selected(self: &Rc<Self>) {
        match self.remote.selected().as_slice() {
            [entry] => self.activate_entry(entry.clone()),
            _ => self.notifier.info(tr!("Select a single file or folder")),
        }
    }

    fn make_dir(self: &Rc<Self>) {
        let cwd = self.remote.cwd();
        let weak = Rc::downgrade(self);
        dialogs::prompt_text(
            &self.root,
            tr!("New Folder"),
            &tr!("Name of the new folder in {}", sftp::display(&cwd)),
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
        let selected = self.remote.selected();
        let [entry] = selected.as_slice() else {
            self.notifier.info(tr!("Select a single item to rename"));
            return;
        };
        let entry = entry.clone();
        let current_name = entry.display_name();
        let current_name_shown = current_name.clone();
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
                if name == current_name_shown {
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
        let entries = top_level(self.remote.selected());
        if entries.is_empty() {
            self.notifier.info(tr!("Select what to delete"));
            return;
        }
        let names: Vec<String> = entries.iter().map(FileEntry::display_name).collect();
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
        let paths: Vec<PathBuf> = entries.into_iter().map(|e| e.path).collect();
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
}

// ------------------------------------------------------------- расчёты

/// Что сделать по «копировать на другую сторону».
#[derive(Debug, PartialEq, Eq)]
enum CopyPlan {
    /// Загрузить локальные пути в каталог сервера.
    Upload {
        paths: Vec<PathBuf>,
        dest: PathBuf,
    },
    /// Скачать элементы сервера в локальный каталог.
    Download {
        entries: Vec<FileEntry>,
        dest: PathBuf,
    },
    NothingSelected,
    /// Обе стороны копирования идут через SFTP.
    NotConnected,
    /// Другое дерево не показывает ни одного каталога.
    NoDestination,
}

/// План копирования выбранного в дереве `from` в каталог `dest` другого
/// дерева. Вложенное в выбранные каталоги отбрасывается, спецфайлы не
/// передаются.
fn copy_plan(from: Side, selected: Vec<FileEntry>, dest: &Path, connected: bool) -> CopyPlan {
    let entries: Vec<FileEntry> = top_level(selected)
        .into_iter()
        .filter(|entry| entry.kind != EntryKind::Other)
        .collect();
    if entries.is_empty() {
        return CopyPlan::NothingSelected;
    }
    if !connected {
        return CopyPlan::NotConnected;
    }
    if dest.as_os_str().is_empty() {
        return CopyPlan::NoDestination;
    }
    let dest = dest.to_path_buf();
    match from {
        Side::Local => CopyPlan::Upload {
            paths: entries.into_iter().map(|entry| entry.path).collect(),
            dest,
        },
        Side::Remote => CopyPlan::Download { entries, dest },
    }
}

/// Имена из `incoming` (имя, это каталог), которые уже заняты в каталоге
/// назначения, и есть ли среди совпавших каталоги (они сливаются).
fn conflicting<'a>(
    incoming: impl IntoIterator<Item = (&'a OsStr, bool)>,
    taken: impl Fn(&OsStr) -> bool,
) -> (Vec<String>, bool) {
    let mut names = Vec::new();
    let mut any_dir = false;
    for (name, is_dir) in incoming {
        if taken(name) {
            names.push(name.to_string_lossy().into_owned());
            any_dir |= is_dir;
        }
    }
    (names, any_dir)
}

#[cfg(test)]
pub(super) fn test_entry(path: &str, kind: EntryKind) -> FileEntry {
    let path = PathBuf::from(path);
    FileEntry {
        name: path.file_name().unwrap_or_default().to_os_string(),
        path,
        kind,
        is_link: false,
        size: Some(1),
        modified: Some(100),
        mode: Some(0o644),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn copy_plan_uploads_top_level_local_selection() {
        let selected = vec![
            test_entry("/home/me/src", EntryKind::Dir),
            test_entry("/home/me/src/main.rs", EntryKind::File),
            test_entry("/home/me/a.txt", EntryKind::File),
            test_entry("/home/me/sock", EntryKind::Other),
        ];
        let plan = copy_plan(Side::Local, selected, Path::new("/srv"), true);
        assert_eq!(
            plan,
            CopyPlan::Upload {
                paths: paths(&["/home/me/src", "/home/me/a.txt"]),
                dest: PathBuf::from("/srv"),
            }
        );
    }

    #[test]
    fn copy_plan_downloads_remote_selection() {
        let dir = test_entry("/srv/www", EntryKind::Dir);
        let file = test_entry("/srv/www/index.html", EntryKind::File);
        let plan = copy_plan(
            Side::Remote,
            vec![file, dir.clone()],
            Path::new("/home/me"),
            true,
        );
        assert_eq!(
            plan,
            CopyPlan::Download {
                entries: vec![dir],
                dest: PathBuf::from("/home/me"),
            }
        );
    }

    #[test]
    fn copy_plan_refusals() {
        let file = || vec![test_entry("/a/b", EntryKind::File)];
        let other = vec![test_entry("/a/fifo", EntryKind::Other)];
        for side in [Side::Local, Side::Remote] {
            assert_eq!(
                copy_plan(side, Vec::new(), Path::new("/d"), true),
                CopyPlan::NothingSelected
            );
            assert_eq!(
                copy_plan(side, other.clone(), Path::new("/d"), true),
                CopyPlan::NothingSelected
            );
            // Без SFTP не скачать и не загрузить.
            assert_eq!(
                copy_plan(side, file(), Path::new("/d"), false),
                CopyPlan::NotConnected
            );
            assert_eq!(
                copy_plan(side, file(), Path::new(""), true),
                CopyPlan::NoDestination
            );
        }
    }

    #[test]
    fn conflicting_names_and_folders() {
        use std::os::unix::ffi::OsStrExt;
        let taken = |name: &OsStr| matches!(name.as_bytes(), b"a.txt" | b"src" | b"\xff");
        let incoming = [
            (OsStr::new("a.txt"), false),
            (OsStr::new("new.txt"), false),
            (OsStr::from_bytes(b"\xff"), false),
        ];
        assert_eq!(
            conflicting(incoming, taken),
            (vec!["a.txt".to_string(), "\u{fffd}".to_string()], false)
        );
        let with_dir = [(OsStr::new("src"), true), (OsStr::new("docs"), true)];
        assert_eq!(
            conflicting(with_dir, taken),
            (vec!["src".to_string()], true)
        );
        // Каталог без совпадения слияния не означает.
        assert_eq!(
            conflicting([(OsStr::new("docs"), true)], taken),
            (Vec::new(), false)
        );
    }
}
