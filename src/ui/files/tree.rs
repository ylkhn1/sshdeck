//! Дерево файлов одной стороны (сервера или этого компьютера): строка
//! навигации, страница состояния, таблица с раскрываемыми каталогами,
//! контекстное меню и подсветка цели перетаскивания.
//!
//! Дерево само ничего не читает: каталоги оно просит у панели через
//! обработчик запросов ([`TreeRequest`]), а ответы панель передаёт обратно
//! вызовами [`FileTree::show_dir`], [`FileTree::navigate_failed`] и
//! [`FileTree::on_listing`]. Так одно и то же дерево работает и поверх
//! SFTP-задачи, и поверх локальной файловой системы.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gdk, gio, glib, graphene, pango};

use super::super::{ANIMATION_MS, format_mtime, format_size};
use crate::sftp::{self, EntryKind, FileEntry};

/// Чьи файлы показывает дерево.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    /// Файлы этого компьютера.
    Local,
    /// Файлы сервера (через SFTP).
    Remote,
}

/// Что дерево просит у панели. Пути — как их понимает своя сторона.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TreeRequest {
    /// Перейти в каталог: путь нормализуется и читается. Ответ —
    /// [`FileTree::show_dir`] или [`FileTree::navigate_failed`].
    Navigate(PathBuf),
    /// Прочитать каталог (раскрытие узла или обновление). Ответ —
    /// [`FileTree::on_listing`]; при ошибке дерево ответа не ждёт.
    List(PathBuf),
}

type RequestHandler = Box<dyn Fn(Side, TreeRequest)>;
type ActivateHandler = Box<dyn Fn(FileEntry)>;
type DragHandler = Box<dyn Fn() -> Option<gdk::ContentProvider>>;

/// Ручка дерева; копии ссылаются на одно и то же дерево.
#[derive(Clone)]
pub(super) struct FileTree {
    inner: Rc<TreeInner>,
}

struct TreeInner {
    side: Side,
    /// Вверх, домой, путь, обновить.
    nav_row: gtk::Box,
    path_entry: gtk::Entry,
    /// Страница состояния или таблица.
    stack: gtk::Stack,
    status_page: adw::StatusPage,
    spinner: adw::Spinner,
    view: gtk::ColumnView,
    selection: gtk::MultiSelection,
    root_store: gio::ListStore,
    context_menu: gtk::PopoverMenu,
    /// Строка, подсвеченная как цель перетаскивания.
    drop_row: RefCell<Option<glib::WeakRef<gtk::Widget>>>,
    on_request: RefCell<Option<RequestHandler>>,
    on_activate: RefCell<Option<ActivateHandler>>,
    on_drag: RefCell<Option<DragHandler>>,
    state: RefCell<TreeState>,
}

#[derive(Default)]
struct TreeState {
    home: PathBuf,
    /// Каталог, показанный в корне дерева.
    cwd: PathBuf,
    show_hidden: bool,
    /// Загруженные каталоги: корень и раскрытые узлы.
    dirs: HashMap<PathBuf, DirNode>,
}

struct DirNode {
    /// Слабая ссылка: модель узла принадлежит дереву и исчезает при
    /// сворачивании.
    store: glib::WeakRef<gio::ListStore>,
    /// Содержимое как пришло (без фильтра скрытых).
    entries: Vec<FileEntry>,
    /// Содержимое уже пришло (пустой список ещё не значит пустой каталог).
    loaded: bool,
}

impl FileTree {
    /// Дерево стороны `side`; `menu` — модель контекстного меню строк
    /// (её действия ищутся от таблицы вверх по виджетам).
    pub(super) fn new(side: Side, menu: &gio::MenuModel) -> Self {
        let inner = Rc::new_cyclic(|weak| TreeInner::build(weak, side, menu));
        Self { inner }
    }

    /// Строка навигации (вверх, домой, путь, обновить). Панель размещает
    /// её сама и может добавить в неё свои кнопки.
    pub(super) fn nav_row(&self) -> &gtk::Box {
        &self.inner.nav_row
    }

    /// Содержимое дерева: страница состояния или таблица. Сюда же панель
    /// вешает приём перетаскивания; координаты [`Self::drop_row_at`] —
    /// относительно этого виджета.
    pub(super) fn content(&self) -> &gtk::Stack {
        &self.inner.stack
    }

    /// Таблица — для горячих клавиш панели.
    pub(super) fn view(&self) -> &gtk::ColumnView {
        &self.inner.view
    }

    /// Куда дерево отправляет запросы каталогов.
    pub(super) fn set_request_handler(&self, handler: impl Fn(Side, TreeRequest) + 'static) {
        *self.inner.on_request.borrow_mut() = Some(Box::new(handler));
    }

    /// Двойной щелчок или Enter по строке (любого типа: и каталога тоже).
    pub(super) fn set_activate_handler(&self, handler: impl Fn(FileEntry) + 'static) {
        *self.inner.on_activate.borrow_mut() = Some(Box::new(handler));
    }

    /// Начало перетаскивания строк наружу: обработчик отдаёт данные для
    /// приёмника (выделение к этому времени уже включает строку под
    /// указателем) или `None`, чтобы не перетаскивать.
    pub(super) fn set_drag_handler(
        &self,
        handler: impl Fn() -> Option<gdk::ContentProvider> + 'static,
    ) {
        *self.inner.on_drag.borrow_mut() = Some(Box::new(handler));
    }

    /// Значок стороны в поле пути (с подсказкой) или `None` — без значка.
    pub(super) fn set_location_icon(&self, icon: Option<(&str, &str)>) {
        let entry = &self.inner.path_entry;
        entry.set_icon_from_icon_name(gtk::EntryIconPosition::Primary, icon.map(|(name, _)| name));
        entry.set_icon_tooltip_text(gtk::EntryIconPosition::Primary, icon.map(|(_, tip)| tip));
    }

    /// Домашний каталог для кнопки «Домой» и путей с `~`.
    pub(super) fn set_home(&self, home: PathBuf) {
        self.inner.state.borrow_mut().home = home;
    }

    /// Каталог в корне дерева; пустой, пока ни один не показан.
    pub(super) fn cwd(&self) -> PathBuf {
        self.inner.state.borrow().cwd.clone()
    }

    /// Просит панель перейти в каталог.
    pub(super) fn navigate(&self, path: &Path) {
        self.inner.navigate(path);
    }

    /// Ответ на [`TreeRequest::Navigate`]: каталог становится корнем.
    pub(super) fn show_dir(&self, path: PathBuf, entries: Vec<FileEntry>) {
        self.inner.show_dir(path, entries);
    }

    /// Перейти не удалось: поле пути снова показывает текущий каталог.
    pub(super) fn navigate_failed(&self) {
        let cwd = self.cwd();
        self.inner.path_entry.set_text(&sftp::display(&cwd));
    }

    /// Ответ на [`TreeRequest::List`].
    pub(super) fn on_listing(&self, path: &Path, entries: Vec<FileEntry>) {
        self.inner.on_listing(path, entries);
    }

    /// Перечитывает все показанные каталоги.
    pub(super) fn refresh_all(&self) {
        self.inner.refresh_all();
    }

    /// Перечитывает каталог, если он показан.
    pub(super) fn refresh_dir(&self, path: &Path) {
        self.inner.refresh_dir(path);
    }

    pub(super) fn set_show_hidden(&self, show: bool) {
        self.inner.state.borrow_mut().show_hidden = show;
        self.inner.refill_all();
    }

    /// Имена в каталоге `dir` (со скрытыми), если его содержимое уже
    /// прочитано.
    pub(super) fn known_names(&self, dir: &Path) -> Option<Vec<OsString>> {
        let state = self.inner.state.borrow();
        let node = state.dirs.get(dir).filter(|node| node.loaded)?;
        Some(node.entries.iter().map(|e| e.name.clone()).collect())
    }

    pub(super) fn selected(&self) -> Vec<FileEntry> {
        self.inner.selected()
    }

    /// Выбранные файлы и каталоги, пригодные для передачи.
    pub(super) fn selected_transferable(&self) -> Vec<FileEntry> {
        self.inner.selected_transferable()
    }

    /// Экран состояния вместо таблицы; `busy` включает индикатор ожидания.
    /// Строка навигации недоступна, пока не вызван [`Self::show_files`].
    pub(super) fn show_status(&self, busy: bool, title: &str, description: &str) {
        let inner = &self.inner;
        inner.status_page.set_title(title);
        // Описание — разметка Pango, поэтому текст экранируется.
        inner
            .status_page
            .set_description(Some(glib::markup_escape_text(description).as_str()));
        inner.spinner.set_visible(busy);
        inner.stack.set_visible_child_name("status");
        inner.nav_row.set_sensitive(false);
    }

    /// Снова таблица вместо экрана состояния.
    pub(super) fn show_files(&self) {
        self.inner.stack.set_visible_child_name("files");
        self.inner.nav_row.set_sensitive(true);
    }

    /// Строка под точкой (координаты [`Self::content`]) и её элемент.
    pub(super) fn drop_row_at(&self, x: f64, y: f64) -> Option<(gtk::Widget, FileEntry)> {
        self.inner.drop_row_at(x, y)
    }

    /// Подсвечивает цель перетаскивания: строку каталога, куда упадут
    /// файлы, или (`whole`, без строки) всё дерево — тогда файлы попадут в
    /// текущий каталог. `(None, false)` снимает подсветку.
    pub(super) fn highlight_drop_target(&self, row: Option<&gtk::Widget>, whole: bool) {
        self.inner.highlight_drop_target(row, whole);
    }

    /// Отцепляет контекстное меню от таблицы (при закрытии вкладки).
    pub(super) fn shutdown(&self) {
        self.inner.context_menu.unparent();
    }
}

// ------------------------------------------------------------ построение

impl TreeInner {
    fn build(weak: &Weak<TreeInner>, side: Side, menu: &gio::MenuModel) -> TreeInner {
        // --- строка навигации
        let home_button = icon_button("go-home-symbolic", tr!("Home folder"));
        let up_button = icon_button("go-up-symbolic", tr!("Up one level"));
        let refresh_button = icon_button("view-refresh-symbolic", tr!("Refresh (Ctrl+R)"));
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

        // --- дерево файлов
        let root_store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let tree = {
            let weak = weak.clone();
            gtk::TreeListModel::new(root_store.clone(), false, false, move |item| {
                let entry = item
                    .downcast_ref::<glib::BoxedAnyObject>()?
                    .borrow::<FileEntry>()
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
        let context_menu = gtk::PopoverMenu::from_model(Some(menu));
        context_menu.set_parent(&view);
        context_menu.set_has_arrow(false);
        context_menu.set_halign(gtk::Align::Start);

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
            refresh_button.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.refresh_all();
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
            view.connect_activate(move |_, position| {
                if let Some(inner) = weak.upgrade()
                    && let Some(entry) = inner.entry_at(position)
                    && let Some(handler) = inner.on_activate.borrow().as_ref()
                {
                    handler(entry);
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

        TreeInner {
            side,
            nav_row,
            path_entry,
            stack,
            status_page,
            spinner,
            view,
            selection,
            root_store,
            context_menu,
            drop_row: RefCell::new(None),
            on_request: RefCell::new(None),
            on_activate: RefCell::new(None),
            on_drag: RefCell::new(None),
            state: RefCell::new(TreeState::default()),
        }
    }
}

/// Передаёт обработчику собственную копию слабой ссылки на дерево.
fn connect(weak: &Weak<TreeInner>, wire: impl FnOnce(Weak<TreeInner>)) {
    wire(weak.clone());
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .build()
}

/// Ищет развёртку строки (`TreeExpander`) в самом виджете или его потомках
/// не глубже `depth` уровней.
fn find_expander(widget: &gtk::Widget, depth: u32) -> Option<gtk::TreeExpander> {
    if let Some(expander) = widget.downcast_ref::<gtk::TreeExpander>() {
        return Some(expander.clone());
    }
    if depth == 0 {
        return None;
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(expander) = find_expander(&current, depth - 1) {
            return Some(expander);
        }
        child = current.next_sibling();
    }
    None
}

fn entry_of_item(item: &gtk::ListItem) -> Option<FileEntry> {
    let row = item.item().and_downcast::<gtk::TreeListRow>()?;
    let object = row.item().and_downcast::<glib::BoxedAnyObject>()?;
    let entry = object.borrow::<FileEntry>().clone();
    Some(entry)
}

fn icon_for(entry: &FileEntry) -> &'static str {
    match entry.kind {
        EntryKind::Dir => "folder-symbolic",
        EntryKind::File => "text-x-generic-symbolic",
        EntryKind::Other => "dialog-question-symbolic",
    }
}

/// Столбец имени: стрелка раскрытия, значок и имя.
fn name_column(weak: &Weak<TreeInner>) -> gtk::ColumnViewColumn {
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
            label.set_text(&entry.display_name());
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
    weak: &Weak<TreeInner>,
    xalign: f32,
    text: impl Fn(&FileEntry) -> String + 'static,
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
        if let (Some(label), Some(entry)) = (
            item.child().and_downcast::<gtk::Label>(),
            entry_of_item(item),
        ) {
            label.set_text(&text(&entry));
        }
    });
    gtk::ColumnViewColumn::new(Some(title), Some(factory))
}

/// Правая кнопка по строке: выделяет её (если не выделена) и показывает меню.
fn attach_context_click(
    widget: &impl IsA<gtk::Widget>,
    item: &gtk::ListItem,
    weak: &Weak<TreeInner>,
) {
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

/// Перетаскивание строки наружу: данные даёт обработчик панели.
fn attach_drag_source(
    widget: &impl IsA<gtk::Widget>,
    item: &gtk::ListItem,
    weak: &Weak<TreeInner>,
) {
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
        let handler = inner.on_drag.borrow();
        handler.as_ref()?()
    });
    // Источник перетаскивания помнит панель до конца перетаскивания.
    source.connect_drag_end(|_, _, _| super::drag::end());
    widget.add_controller(source);
}

/// Порядок строк: каталоги сверху, затем имя без учёта регистра, затем
/// байты имени (чтобы порядок был полным).
fn sort_key(entry: &FileEntry) -> (bool, String, &[u8]) {
    use std::os::unix::ffi::OsStrExt;
    (
        entry.kind != EntryKind::Dir,
        entry.display_name().to_lowercase(),
        entry.name.as_bytes(),
    )
}

/// Приводит модель к новому списку, трогая только изменившиеся строки:
/// так раскрытые подкаталоги остаются раскрытыми при обновлении.
/// И модель, и `entries` упорядочены по [`sort_key`].
fn sync_store(store: &gio::ListStore, entries: &[FileEntry]) {
    let mut position = 0u32;
    let mut index = 0usize;
    loop {
        let existing = store.item(position).and_downcast::<glib::BoxedAnyObject>();
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
                    let old = object.borrow::<FileEntry>();
                    let both_dirs = old.kind == EntryKind::Dir && new.kind == EntryKind::Dir;
                    (
                        sort_key(&old).cmp(&sort_key(new)),
                        *old != *new && !both_dirs,
                    )
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
pub(super) fn top_level(entries: Vec<FileEntry>) -> Vec<FileEntry> {
    let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
    entries
        .into_iter()
        .filter(|entry| {
            !paths
                .iter()
                .any(|other| sftp::is_inside(&entry.path, other))
        })
        .collect()
}

// ------------------------------------------------------------- поведение

impl TreeInner {
    fn request(&self, request: TreeRequest) {
        if let Some(handler) = self.on_request.borrow().as_ref() {
            handler(self.side, request);
        }
    }

    fn navigate(&self, path: &Path) {
        self.request(TreeRequest::Navigate(path.to_path_buf()));
    }

    /// Превращает введённый путь в абсолютный: `~`, относительные пути.
    fn resolve_input(&self, input: &str) -> PathBuf {
        let state = self.state.borrow();
        let input = input.trim();
        // Поле показывает путь с заменёнными недопустимыми байтами; если его
        // не правили, берётся настоящий путь.
        if input.is_empty() || input == sftp::display(&state.cwd) {
            state.cwd.clone()
        } else if input == "~" {
            state.home.clone()
        } else if let Some(rest) = input.strip_prefix("~/") {
            sftp::join_remote(&state.home, rest)
        } else if input.starts_with('/') {
            PathBuf::from(input)
        } else {
            sftp::join_remote(&state.cwd, input)
        }
    }

    fn show_dir(&self, path: PathBuf, entries: Vec<FileEntry>) {
        {
            let mut state = self.state.borrow_mut();
            state.cwd = path.clone();
            state.dirs.clear();
            state.dirs.insert(
                path.clone(),
                DirNode {
                    store: self.root_store.downgrade(),
                    entries,
                    loaded: true,
                },
            );
        }
        self.root_store.remove_all();
        self.path_entry.set_text(&sftp::display(&path));
        self.fill(&path, &self.root_store);
    }

    /// Узел дерева раскрыт: запоминаем его модель и запрашиваем содержимое.
    fn load_dir(&self, path: &Path, store: &gio::ListStore) {
        self.state.borrow_mut().dirs.insert(
            path.to_path_buf(),
            DirNode {
                store: store.downgrade(),
                entries: Vec::new(),
                loaded: false,
            },
        );
        self.request(TreeRequest::List(path.to_path_buf()));
    }

    fn on_listing(&self, path: &Path, entries: Vec<FileEntry>) {
        let store = {
            let mut state = self.state.borrow_mut();
            let store = state.dirs.get(path).and_then(|node| node.store.upgrade());
            match (&store, state.dirs.get_mut(path)) {
                (Some(_), Some(node)) => {
                    node.entries = entries;
                    node.loaded = true;
                }
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

    fn fill(&self, path: &Path, store: &gio::ListStore) {
        let visible = {
            let state = self.state.borrow();
            let Some(node) = state.dirs.get(path) else {
                return;
            };
            let mut visible: Vec<FileEntry> = node
                .entries
                .iter()
                .filter(|entry| state.show_hidden || !entry.is_hidden())
                .cloned()
                .collect();
            visible.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
            visible
        };
        sync_store(store, &visible);
    }

    /// Живые (показанные) каталоги; заодно забывает свёрнутые.
    fn live_dirs(&self) -> Vec<(PathBuf, gio::ListStore)> {
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
            self.request(TreeRequest::List(path));
        }
    }

    fn refresh_dir(&self, path: &Path) {
        let shown = self
            .state
            .borrow()
            .dirs
            .get(path)
            .is_some_and(|node| node.store.upgrade().is_some());
        if shown {
            self.request(TreeRequest::List(path.to_path_buf()));
        }
    }

    // --- перетаскивание внутрь

    /// Строка под точкой (координаты стека) и её элемент.
    ///
    /// У ColumnView нет вызова «строка в точке», поэтому берётся виджет под
    /// указателем и поднимаемся к предкам, пока среди ближайших потомков не
    /// найдётся развёртка строки: у неё есть ссылка на элемент модели.
    /// Строка таблицы — предок развёртки с CSS-именем `row`.
    fn drop_row_at(&self, x: f64, y: f64) -> Option<(gtk::Widget, FileEntry)> {
        let point = self
            .stack
            .compute_point(&self.view, &graphene::Point::new(x as f32, y as f32))?;
        let mut widget = self.view.pick(
            f64::from(point.x()),
            f64::from(point.y()),
            gtk::PickFlags::DEFAULT,
        )?;
        let view: &gtk::Widget = self.view.upcast_ref();
        let expander = loop {
            if widget == *view {
                return None;
            }
            // Строка → ячейка → развёртка: два уровня. Глубже искать нельзя,
            // иначе со списка целиком нашлась бы первая строка.
            if let Some(expander) = find_expander(&widget, 2) {
                break expander;
            }
            widget = widget.parent()?;
        };
        let entry = expander
            .list_row()?
            .item()
            .and_downcast::<glib::BoxedAnyObject>()?
            .borrow::<FileEntry>()
            .clone();
        let mut row: gtk::Widget = expander.clone().upcast();
        for _ in 0..3 {
            if row.css_name() == "row" {
                break;
            }
            match row.parent() {
                Some(parent) if parent != *view => row = parent,
                _ => break,
            }
        }
        Some((row, entry))
    }

    fn highlight_drop_target(&self, row: Option<&gtk::Widget>, whole: bool) {
        let previous = self
            .drop_row
            .borrow_mut()
            .take()
            .and_then(|weak| weak.upgrade());
        if let Some(previous) = previous {
            previous.unset_state_flags(gtk::StateFlags::DROP_ACTIVE);
        }
        if let Some(row) = row {
            row.set_state_flags(gtk::StateFlags::DROP_ACTIVE, false);
            *self.drop_row.borrow_mut() = Some(row.downgrade());
        }
        if whole && row.is_none() {
            self.stack
                .set_state_flags(gtk::StateFlags::DROP_ACTIVE, false);
        } else {
            self.stack.unset_state_flags(gtk::StateFlags::DROP_ACTIVE);
        }
    }

    // --- выделение

    fn entry_at(&self, position: u32) -> Option<FileEntry> {
        let row = self
            .selection
            .item(position)
            .and_downcast::<gtk::TreeListRow>()?;
        let object = row.item().and_downcast::<glib::BoxedAnyObject>()?;
        let entry = object.borrow::<FileEntry>().clone();
        Some(entry)
    }

    fn selected(&self) -> Vec<FileEntry> {
        let bitset = self.selection.selection();
        let count = u32::try_from(bitset.size()).unwrap_or(u32::MAX);
        (0..count)
            .filter_map(|index| self.entry_at(bitset.nth(index)))
            .collect()
    }

    fn selected_transferable(&self) -> Vec<FileEntry> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: PathBuf::from("/d").join(name),
            kind,
            is_link: false,
            size: Some(1),
            modified: Some(100),
            mode: Some(0o644),
        }
    }

    fn dir(name: &str) -> FileEntry {
        entry(name, EntryKind::Dir)
    }

    fn file(name: &str) -> FileEntry {
        entry(name, EntryKind::File)
    }

    fn sorted(mut entries: Vec<FileEntry>) -> Vec<FileEntry> {
        entries.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        entries
    }

    fn objects(store: &gio::ListStore) -> Vec<glib::BoxedAnyObject> {
        (0..store.n_items())
            .filter_map(|i| store.item(i).and_downcast::<glib::BoxedAnyObject>())
            .collect()
    }

    fn contents(store: &gio::ListStore) -> Vec<FileEntry> {
        objects(store)
            .iter()
            .map(|object| object.borrow::<FileEntry>().clone())
            .collect()
    }

    /// Объект строки с этим именем (ищется по имени, а не по позиции).
    fn object_named(store: &gio::ListStore, name: &str) -> glib::BoxedAnyObject {
        objects(store)
            .into_iter()
            .find(|object| object.borrow::<FileEntry>().name == *name)
            .expect("row")
    }

    #[test]
    fn sort_key_puts_folders_first_and_ignores_case() {
        let names: Vec<String> = sorted(vec![
            file("b.txt"),
            dir("zeta"),
            file("A.txt"),
            dir("Alpha"),
            file("a.txt"),
            entry("sock", EntryKind::Other),
        ])
        .iter()
        .map(FileEntry::display_name)
        .collect();
        // «A.txt» и «a.txt» равны без учёта регистра; дальше решают байты.
        assert_eq!(names, ["Alpha", "zeta", "A.txt", "a.txt", "b.txt", "sock"]);
    }

    #[test]
    fn sort_key_orders_non_utf8_names() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let mut bad = file("x");
        bad.name = OsStr::from_bytes(b"\xff").to_os_string();
        let mut bad2 = file("x");
        bad2.name = OsStr::from_bytes(b"\xfe").to_os_string();
        // Оба показываются как «�»; порядок всё равно полный и устойчивый.
        let order = sorted(vec![bad.clone(), bad2.clone()]);
        assert_eq!(order, [bad2, bad]);
    }

    #[test]
    fn sync_store_fills_empty_and_clears() {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let entries = sorted(vec![file("b"), dir("a"), file("c")]);
        sync_store(&store, &entries);
        assert_eq!(contents(&store), entries);
        sync_store(&store, &[]);
        assert_eq!(store.n_items(), 0);
    }

    #[test]
    fn sync_store_keeps_unchanged_rows() {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let entries = sorted(vec![dir("src"), file("a"), file("b")]);
        sync_store(&store, &entries);
        let before = objects(&store);
        sync_store(&store, &entries);
        assert_eq!(objects(&store), before);
    }

    #[test]
    fn sync_store_inserts_removes_and_replaces() {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        sync_store(
            &store,
            &sorted(vec![
                dir("docs"),
                dir("old"),
                file("keep"),
                file("gone"),
                file("edit"),
            ]),
        );
        let docs = object_named(&store, "docs");
        let keep = object_named(&store, "keep");
        let edit = object_named(&store, "edit");

        let mut edited = file("edit");
        edited.size = Some(2);
        // Каталог с другим временем изменения — тот же узел (раскрытие
        // сохраняется), файл с другим размером — новая строка.
        let mut docs_touched = dir("docs");
        docs_touched.modified = Some(200);
        let next = sorted(vec![
            docs_touched,
            dir("new"),
            file("keep"),
            edited.clone(),
            file("added"),
        ]);
        sync_store(&store, &next);

        let rows = contents(&store);
        let names: Vec<String> = rows.iter().map(FileEntry::display_name).collect();
        assert_eq!(names, ["docs", "new", "added", "edit", "keep"]);
        assert_eq!(object_named(&store, "docs"), docs);
        assert_eq!(object_named(&store, "keep"), keep);
        assert_ne!(object_named(&store, "edit"), edit);
        assert_eq!(rows[3], edited);
    }

    #[test]
    fn sync_store_handles_reorder_by_kind() {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        sync_store(&store, &sorted(vec![dir("x"), file("y")]));
        // «x» стал файлом, «y» — каталогом: порядок меняется.
        let next = sorted(vec![file("x"), dir("y")]);
        sync_store(&store, &next);
        assert_eq!(contents(&store), next);
    }

    #[test]
    fn top_level_drops_nested_selection() {
        let mut nested = file("inner");
        nested.path = PathBuf::from("/d/sub/inner");
        let mut deeper = file("deep");
        deeper.path = PathBuf::from("/d/sub/x/deep");
        let mut similar = file("sub2");
        similar.path = PathBuf::from("/d/sub2");
        let picked = top_level(vec![nested, dir("sub"), deeper, similar.clone(), file("a")]);
        let paths: Vec<PathBuf> = picked.into_iter().map(|e| e.path).collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("/d/sub"),
                PathBuf::from("/d/sub2"),
                PathBuf::from("/d/a")
            ]
        );
    }

    #[test]
    fn top_level_under_root() {
        let mut root = dir("");
        root.path = PathBuf::from("/");
        let picked = top_level(vec![file("a"), root.clone()]);
        assert_eq!(picked, [root]);
        assert_eq!(top_level(vec![file("a"), file("b")]).len(), 2);
    }
}
