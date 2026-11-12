//! Вкладка базы данных: слева дерево (базы → схемы → таблицы → колонки) с
//! размерами таблиц, справа SQL-редактор и таблица результатов.
//!
//! Ctrl+Enter выполняет запрос под курсором (или выделенный текст),
//! Ctrl+Shift+Enter — весь текст редактора. Двойной щелчок по таблице
//! показывает её первые строки, правая кнопка — меню (структура, экспорт,
//! импорт). Двойной щелчок по базе делает её активной: запросы редактора
//! выполняются в ней.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};

use super::databases::{Databases, profile_badge};
use super::{ANIMATION_MS, Notifier, format_size};
use crate::db::{
    self, Catalog, ColumnInfo, ConnectRequest, DatabaseInfo, DbKind, DbProfile, DbWorker, Event,
    ExportFormat, ImportOptions, MAX_ROWS, QueryOutput, Request, TableInfo, TransferReport,
};

/// Сколько строк показывать при щелчке по таблице.
const PREVIEW_ROWS: usize = 200;
/// Длиннее этого значение в ячейке обрезается (целиком оно видно по
/// двойному щелчку на строке).
const CELL_PREVIEW_CHARS: usize = 200;
const NAV_WIDTH: i32 = 280;
const EDITOR_HEIGHT: i32 = 200;
const LOADING: &str = "Загрузка…";

const CANCEL: &str = "cancel";
const ACCEPT: &str = "accept";

/// Ищет сокет мастер-соединения открытой вкладки к SSH-серверу (адрес уже
/// проверен `validate_target`).
pub type ControlLookup = Rc<dyn Fn(&str) -> Option<PathBuf>>;

/// База, схема, таблица.
type TableKey = (String, String, String);

// ------------------------------------------------------------------ дерево

#[derive(Clone)]
enum NodeKind {
    Database(DatabaseInfo),
    Schema {
        database: String,
        name: String,
        /// Сумма размеров таблиц.
        size: Option<u64>,
        tables: usize,
    },
    Table {
        database: String,
        info: TableInfo,
    },
    Column(ColumnInfo),
    /// «Загрузка…», «Нет таблиц», ошибка.
    Note(String),
}

/// Узел дерева. Дети хранятся в своей модели, которую показывает
/// `TreeListModel`; у раскрываемых узлов она есть всегда (хотя бы с
/// «Загрузка…»).
struct Node {
    kind: NodeKind,
    children: RefCell<Option<gio::ListStore>>,
}

impl Node {
    fn new(kind: NodeKind) -> Rc<Self> {
        Rc::new(Self {
            kind,
            children: RefCell::new(None),
        })
    }

    fn with_children(kind: NodeKind, children: &[Rc<Node>]) -> Rc<Self> {
        let node = Self::new(kind);
        node.set_children(children);
        node
    }

    fn note(text: &str) -> Rc<Self> {
        Self::new(NodeKind::Note(text.to_string()))
    }

    fn expandable(&self) -> bool {
        matches!(
            self.kind,
            NodeKind::Database(_) | NodeKind::Schema { .. } | NodeKind::Table { .. }
        )
    }

    fn store(&self) -> gio::ListStore {
        self.children
            .borrow_mut()
            .get_or_insert_with(gio::ListStore::new::<glib::BoxedAnyObject>)
            .clone()
    }

    fn set_children(&self, children: &[Rc<Node>]) {
        let store = self.store();
        let items: Vec<glib::BoxedAnyObject> = children.iter().map(boxed).collect();
        store.splice(0, store.n_items(), &items);
    }

    /// Ключ для запоминания раскрытых узлов между перестройками.
    fn key(&self) -> String {
        match &self.kind {
            NodeKind::Database(db) => format!("db\u{1}{}", db.name),
            NodeKind::Schema { database, name, .. } => format!("schema\u{1}{database}\u{1}{name}"),
            NodeKind::Table { database, info } => {
                format!("table\u{1}{database}\u{1}{}\u{1}{}", info.schema, info.name)
            }
            NodeKind::Column(column) => format!("column\u{1}{}", column.name),
            NodeKind::Note(text) => format!("note\u{1}{text}"),
        }
    }
}

fn boxed(node: &Rc<Node>) -> glib::BoxedAnyObject {
    glib::BoxedAnyObject::new(Rc::clone(node))
}

fn node_of(object: &glib::Object) -> Option<Rc<Node>> {
    object
        .downcast_ref::<glib::BoxedAnyObject>()
        .map(|boxed| Rc::clone(&boxed.borrow::<Rc<Node>>()))
}

fn row_node(row: &gtk::TreeListRow) -> Option<Rc<Node>> {
    row.item().as_ref().and_then(node_of)
}

/// Последний запрос из редактора или дерева — для экспорта результата.
#[derive(Clone)]
struct LastQuery {
    sql: String,
    database: String,
    /// Имя таблицы для `INSERT INTO` (в кавычках), если запрос — просмотр
    /// таблицы.
    table: Option<String>,
    /// Основа имени файла.
    stem: Option<String>,
}

/// Что выгружать.
struct ExportSource {
    sql: String,
    database: String,
    table: String,
    stem: String,
    description: String,
    /// Уже полученные строки: тогда запрос заново не выполняется.
    local: Option<Rc<QueryOutput>>,
}

/// Выполняющийся экспорт или импорт.
struct Transfer {
    import: bool,
    database: String,
    target: String,
}

pub struct DbTab {
    me: Weak<DbTab>,
    notifier: Notifier,
    profile: DbProfile,
    password: RefCell<String>,
    control: ControlLookup,
    /// Сохранённые базы: через них спрашивается новый пароль, если сервер
    /// не принял прежний.
    store: Databases,
    /// Последнее подключение не удалось из-за пароля.
    auth_failed: Cell<bool>,
    worker: RefCell<Option<DbWorker>>,
    /// Номер подключения: события от прошлых подключений отбрасываются.
    generation: Cell<u64>,
    connected: Cell<bool>,
    /// Запрос (или экспорт, импорт), результата которого ждём.
    pending: Cell<Option<u64>>,
    next_id: Cell<u64>,
    transfer: RefCell<Option<Transfer>>,
    last_query: RefCell<Option<LastQuery>>,
    /// Последний выбранный формат экспорта.
    export_format: Cell<u32>,

    banner: adw::Banner,
    status: gtk::Label,
    /// «PostgreSQL 16 · через SSH …» без активной базы.
    status_base: RefCell<String>,
    spinner: adw::Spinner,
    run_button: gtk::Button,
    stop_button: gtk::Button,
    editor: gtk::TextView,
    executed_tag: gtk::TextTag,
    /// Теги подсветки SQL: ключевые слова, строки, числа, комментарии.
    syntax_tags: [gtk::TextTag; 4],
    /// Подсветка уже запланирована на ближайший простой.
    highlight_scheduled: Cell<bool>,

    // --- дерево
    search: gtk::SearchEntry,
    tree: gtk::ListView,
    tree_model: gtk::TreeListModel,
    root: gio::ListStore,
    nav_scroll: gtk::ScrolledWindow,
    /// Надпись вместо пустого дерева.
    nav_placeholder: gtk::Label,
    /// Что писать, когда таблиц нет: «Подключение…», «В базе нет таблиц»…
    nav_note: RefCell<String>,
    nav_menu: gtk::PopoverMenu,
    menu_node: RefCell<Option<Rc<Node>>>,
    /// База, к которой подключились, и база для запросов редактора.
    main_db: RefCell<String>,
    active_db: RefCell<String>,
    databases: RefCell<Vec<DatabaseInfo>>,
    catalogs: RefCell<HashMap<String, Catalog>>,
    catalog_errors: RefCell<HashMap<String, String>>,
    requested_catalogs: RefCell<HashSet<String>>,
    columns: RefCell<HashMap<TableKey, Result<Vec<ColumnInfo>, String>>>,
    requested_columns: RefCell<HashSet<TableKey>>,
    /// Узлы в дереве, чьих детей мы ждём.
    db_nodes: RefCell<HashMap<String, Rc<Node>>>,
    table_nodes: RefCell<HashMap<TableKey, Rc<Node>>>,
    /// Надписи баз в дереве: у активной — жирный шрифт.
    db_labels: RefCell<HashMap<String, glib::WeakRef<gtk::Label>>>,
    /// Узлы, которые раскрыть при следующей перестройке.
    want_expanded: RefCell<HashSet<String>>,

    results: Rc<Results>,
}

impl DbTab {
    pub fn new(
        profile: DbProfile,
        password: String,
        control: ControlLookup,
        databases: Databases,
    ) -> Rc<Self> {
        let notifier = Notifier::new();

        // --- дерево
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Поиск таблиц")
            .hexpand(true)
            .build();
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Перечитать базы и таблицы")
            .css_classes(["flat"])
            .build();
        let nav_header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        nav_header.add_css_class("sshdeck-db-toolbar");
        nav_header.append(&search);
        nav_header.append(&refresh);
        let nav_placeholder = gtk::Label::builder()
            .label("Подключение…")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .margin_top(24)
            .margin_start(12)
            .margin_end(12)
            .css_classes(["dim-label"])
            .build();
        let root = gio::ListStore::new::<glib::BoxedAnyObject>();
        let tree_model = gtk::TreeListModel::new(root.clone(), false, false, |item| {
            let node = node_of(item)?;
            node.expandable()
                .then(|| node.store().upcast::<gio::ListModel>())
        });
        let selection = gtk::SingleSelection::builder()
            .model(&tree_model)
            .autoselect(false)
            .can_unselect(true)
            .build();
        let tree = gtk::ListView::builder()
            .model(&selection)
            .factory(&gtk::SignalListItemFactory::new())
            .css_classes(["navigation-sidebar", "sshdeck-db-nav"])
            .build();
        let nav_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        nav_content.append(&tree);
        nav_content.append(&nav_placeholder);
        let nav_scroll = gtk::ScrolledWindow::builder()
            .child(&nav_content)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let nav_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        nav_box.append(&nav_header);
        nav_box.append(&nav_scroll);

        let nav_menu = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
        nav_menu.set_has_arrow(false);
        nav_menu.set_parent(&nav_scroll);

        // --- редактор
        let buffer = gtk::TextBuffer::new(None);
        let executed_tag = buffer
            .create_tag(
                Some("executed"),
                &[("background-rgba", &gdk::RGBA::new(0.36, 0.55, 1.0, 0.16))],
            )
            .expect("новый буфер без тегов");
        let syntax_tag = |name: &str, color: &str, bold: bool, italic: bool| {
            let tag = gtk::TextTag::builder().name(name).foreground(color).build();
            if bold {
                tag.set_weight(700);
            }
            if italic {
                tag.set_style(pango::Style::Italic);
            }
            buffer.tag_table().add(&tag);
            tag
        };
        let syntax_tags = [
            syntax_tag("keyword", "#5b8def", true, false),
            syntax_tag("string", "#3fa66a", false, false),
            syntax_tag("number", "#d0892e", false, false),
            syntax_tag("comment", "#8b93a1", false, true),
        ];
        let editor = gtk::TextView::builder()
            .buffer(&buffer)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::None)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(10)
            .right_margin(10)
            .css_classes(["sshdeck-sql"])
            .build();
        let editor_scroll = gtk::ScrolledWindow::builder()
            .child(&editor)
            .vexpand(true)
            .build();
        // Подсказка в пустом редакторе.
        let editor_hint = gtk::Label::builder()
            .label(
                "SELECT * FROM …\n\n\
                 Ctrl+Enter — выполнить запрос под курсором или выделение\n\
                 Ctrl+Shift+Enter — весь текст",
            )
            .xalign(0.0)
            .yalign(0.0)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_top(8)
            .margin_start(12)
            .can_target(false)
            .css_classes(["sshdeck-editor-hint"])
            .build();
        let editor_overlay = gtk::Overlay::builder()
            .child(&editor_scroll)
            .vexpand(true)
            .build();
        editor_overlay.add_overlay(&editor_hint);
        buffer
            .bind_property("text", &editor_hint, "visible")
            .transform_to(|_, text: String| Some(text.is_empty()))
            .sync_create()
            .build();

        let run_button = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("media-playback-start-symbolic")
                    .label("Выполнить")
                    .build(),
            )
            .tooltip_text(
                "Выполнить запрос под курсором или выделенный текст (Ctrl+Enter)\n\
                 Весь текст редактора — Ctrl+Shift+Enter",
            )
            .css_classes(["flat"])
            .sensitive(false)
            .build();
        let stop_button = gtk::Button::builder()
            .icon_name("media-playback-stop-symbolic")
            .tooltip_text("Прервать запрос, экспорт или импорт")
            .css_classes(["flat"])
            .sensitive(false)
            .build();
        let spinner = adw::Spinner::builder().visible(false).build();
        let status = gtk::Label::builder()
            .label("Подключение…")
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let reconnect = gtk::Button::builder()
            .icon_name("network-transmit-receive-symbolic")
            .tooltip_text("Переподключиться")
            .css_classes(["flat"])
            .build();
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        toolbar.add_css_class("sshdeck-db-toolbar");
        toolbar.append(&run_button);
        toolbar.append(&stop_button);
        toolbar.append(&spinner);
        toolbar.append(&profile_badge(&profile));
        toolbar.append(&status);
        toolbar.append(&reconnect);
        let editor_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        editor_box.append(&toolbar);
        editor_box.append(&editor_overlay);

        // --- результаты
        let results = Results::new();

        let work = gtk::Paned::builder()
            .orientation(gtk::Orientation::Vertical)
            .start_child(&editor_box)
            .end_child(&results.root)
            .position(EDITOR_HEIGHT)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .css_classes(["sshdeck-split"])
            .build();
        let split = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&nav_box)
            .end_child(&work)
            .position(NAV_WIDTH)
            .resize_start_child(false)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .vexpand(true)
            .css_classes(["sshdeck-split"])
            .build();
        let banner = adw::Banner::builder()
            .button_label("Переподключиться")
            .revealed(false)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&banner);
        content.append(&split);
        notifier.set_child(&content);

        let tab = Rc::new_cyclic(|me| Self {
            me: me.clone(),
            notifier,
            profile,
            password: RefCell::new(password),
            control,
            store: databases,
            auth_failed: Cell::new(false),
            worker: RefCell::new(None),
            generation: Cell::new(0),
            connected: Cell::new(false),
            pending: Cell::new(None),
            next_id: Cell::new(1),
            transfer: RefCell::new(None),
            last_query: RefCell::new(None),
            export_format: Cell::new(0),
            banner,
            status,
            status_base: RefCell::new(String::new()),
            spinner,
            run_button,
            stop_button,
            editor,
            executed_tag,
            syntax_tags,
            highlight_scheduled: Cell::new(false),
            search,
            tree,
            tree_model,
            root,
            nav_scroll,
            nav_placeholder,
            nav_note: RefCell::new("Подключение…".to_string()),
            nav_menu,
            menu_node: RefCell::new(None),
            main_db: RefCell::new(String::new()),
            active_db: RefCell::new(String::new()),
            databases: RefCell::new(Vec::new()),
            catalogs: RefCell::new(HashMap::new()),
            catalog_errors: RefCell::new(HashMap::new()),
            requested_catalogs: RefCell::new(HashSet::new()),
            columns: RefCell::new(HashMap::new()),
            requested_columns: RefCell::new(HashSet::new()),
            db_nodes: RefCell::new(HashMap::new()),
            table_nodes: RefCell::new(HashMap::new()),
            db_labels: RefCell::new(HashMap::new()),
            want_expanded: RefCell::new(HashSet::new()),
            results,
        });
        tab.connect_signals(&content, &refresh, &reconnect);
        tab.connect_tree();
        tab.connect();
        tab
    }

    fn me(&self) -> Option<Rc<Self>> {
        self.me.upgrade()
    }

    pub fn widget(&self) -> &adw::ToastOverlay {
        self.notifier.widget()
    }

    pub fn focus_editor(&self) {
        self.editor.grab_focus();
    }

    /// Закрывает соединение (и туннель) при закрытии вкладки.
    pub fn shutdown(&self) {
        self.generation.set(self.generation.get() + 1);
        self.worker.take();
        self.nav_menu.unparent();
    }

    fn connect_signals(
        self: &Rc<Self>,
        content: &gtk::Box,
        refresh: &gtk::Button,
        reconnect: &gtk::Button,
    ) {
        let with_tab = |action: fn(&Rc<Self>)| {
            let weak = Rc::downgrade(self);
            move || {
                if let Some(tab) = weak.upgrade() {
                    action(&tab);
                }
            }
        };

        {
            let run = with_tab(|tab| tab.run_from_editor(false));
            self.run_button.connect_clicked(move |_| run());
        }
        {
            let stop = with_tab(|tab| tab.cancel());
            self.stop_button.connect_clicked(move |_| stop());
        }
        {
            let again = with_tab(|tab| tab.connect());
            reconnect.connect_clicked(move |_| again());
            let banner = with_tab(|tab| {
                if tab.auth_failed.get() {
                    tab.ask_password();
                } else {
                    tab.connect();
                }
            });
            self.banner.connect_button_clicked(move |_| banner());
        }
        {
            let reload = with_tab(|tab| tab.refresh_tree());
            refresh.connect_clicked(move |_| reload());
        }
        {
            let rebuild = with_tab(|tab| tab.rebuild_tree());
            self.search.connect_search_changed(move |_| rebuild());
        }
        {
            let export = with_tab(|tab| tab.export_results());
            self.results
                .export_button
                .connect_clicked(move |_| export());
        }
        {
            // Выполненный запрос подсвечивается, пока текст не изменят;
            // подсветка синтаксиса обновляется при простое.
            let weak = Rc::downgrade(self);
            self.editor.buffer().connect_changed(move |buffer| {
                let Some(tab) = weak.upgrade() else { return };
                let (start, end) = buffer.bounds();
                buffer.remove_tag(&tab.executed_tag, &start, &end);
                tab.schedule_highlight();
            });
        }
        {
            // Ctrl+Enter — где бы ни был фокус внутри вкладки.
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            let weak = Rc::downgrade(self);
            keys.connect_key_pressed(move |_, key, _, modifiers| {
                let enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
                if !enter || !modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
                    return glib::Propagation::Proceed;
                }
                if let Some(tab) = weak.upgrade() {
                    tab.run_from_editor(modifiers.contains(gdk::ModifierType::SHIFT_MASK));
                }
                glib::Propagation::Stop
            });
            content.add_controller(keys);
        }
        {
            let actions = gio::SimpleActionGroup::new();
            let add = |name: &str, action: fn(&Self, &Rc<Node>)| {
                let weak = Rc::downgrade(self);
                let entry = gio::SimpleAction::new(name, None);
                entry.connect_activate(move |_, _| {
                    let Some(tab) = weak.upgrade() else { return };
                    let node = tab.menu_node.borrow().clone();
                    if let Some(node) = node {
                        action(&tab, &node);
                    }
                });
                actions.add_action(&entry);
            };
            add("data", |tab, node| {
                if let NodeKind::Table { database, info } = &node.kind {
                    tab.open_data(database, info);
                }
            });
            add("structure", |tab, node| {
                if let NodeKind::Table { database, info } = &node.kind {
                    tab.open_structure(database, info);
                }
            });
            add("export", |tab, node| {
                if let NodeKind::Table { database, info } = &node.kind {
                    tab.export_table(database, info);
                }
            });
            add("import", |tab, node| {
                if let NodeKind::Table { database, info } = &node.kind {
                    tab.import_table(database, info);
                }
            });
            add("activate", |tab, node| {
                if let NodeKind::Database(info) = &node.kind {
                    tab.set_active_database(&info.name);
                }
            });
            add("refresh", |tab, _| tab.refresh_tree());
            add("insert-name", |tab, node| {
                if let Some(name) = tab.node_name(node) {
                    tab.editor.buffer().insert_at_cursor(&name);
                    tab.editor.grab_focus();
                }
            });
            add("copy-name", |tab, node| {
                if let Some(name) = tab.node_name(node) {
                    tab.editor.clipboard().set_text(&name);
                    tab.notifier.info(&format!("Скопировано: {name}"));
                }
            });
            self.widget().insert_action_group("dbnav", Some(&actions));
        }
        {
            let weak = Rc::downgrade(&self.results);
            self.results.view.connect_activate(move |_, position| {
                if let Some(results) = weak.upgrade() {
                    results.show_row(position);
                }
            });
        }
    }

    /// Строки дерева: вид, двойной щелчок, меню и подгрузка при раскрытии.
    fn connect_tree(self: &Rc<Self>) {
        let Some(factory) = self
            .tree
            .factory()
            .and_downcast::<gtk::SignalListItemFactory>()
        else {
            return;
        };
        {
            let weak = Rc::downgrade(self);
            factory.connect_setup(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let icon = gtk::Image::new();
                let name = gtk::Label::builder()
                    .xalign(0.0)
                    .hexpand(true)
                    .ellipsize(pango::EllipsizeMode::End)
                    .build();
                let caption = gtk::Label::builder()
                    .css_classes(["dim-label", "sshdeck-tree-size"])
                    .build();
                let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                content.append(&icon);
                content.append(&name);
                content.append(&caption);
                let expander = gtk::TreeExpander::builder()
                    .child(&content)
                    .indent_for_icon(true)
                    .build();
                let click = gtk::GestureClick::builder()
                    .button(gdk::BUTTON_SECONDARY)
                    .build();
                let weak = weak.clone();
                let weak_item = item.downgrade();
                click.connect_pressed(move |gesture, _, x, y| {
                    let (Some(tab), Some(item)) = (weak.upgrade(), weak_item.upgrade()) else {
                        return;
                    };
                    if let Some(widget) = gesture.widget() {
                        tab.show_node_menu(&item, &widget, x, y);
                    }
                });
                expander.add_controller(click);
                item.set_child(Some(&expander));
            });
        }
        {
            let weak = Rc::downgrade(self);
            factory.connect_bind(move |_, item| {
                let Some(tab) = weak.upgrade() else { return };
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let (Some(expander), Some(row)) = (
                    item.child().and_downcast::<gtk::TreeExpander>(),
                    item.item().and_downcast::<gtk::TreeListRow>(),
                ) else {
                    return;
                };
                expander.set_list_row(Some(&row));
                if let Some(node) = row_node(&row) {
                    tab.bind_node(&expander, &node);
                }
            });
        }
        factory.connect_unbind(|_, item| {
            if let Some(expander) = item
                .downcast_ref::<gtk::ListItem>()
                .and_then(|item| item.child())
                .and_downcast::<gtk::TreeExpander>()
            {
                expander.set_list_row(None);
            }
        });
        {
            let weak = Rc::downgrade(self);
            self.tree.connect_activate(move |_, position| {
                let Some(tab) = weak.upgrade() else { return };
                if let Some(row) = tab.tree_model.row(position)
                    && let Some(node) = row_node(&row)
                {
                    tab.activate_node(&row, &node);
                }
            });
        }
        {
            // Раскрытый узел вставляет детей сразу за собой: если это
            // «Загрузка…», пора просить данные.
            let weak = Rc::downgrade(self);
            self.tree_model
                .connect_items_changed(move |model, position, _, added| {
                    let Some(tab) = weak.upgrade() else { return };
                    if added == 0 || position == 0 {
                        return;
                    }
                    if let Some(row) = model.row(position - 1)
                        && row.is_expanded()
                        && let Some(node) = row_node(&row)
                    {
                        tab.ensure_loaded(&node);
                    }
                });
        }
    }

    /// Заполняет строку дерева.
    fn bind_node(&self, expander: &gtk::TreeExpander, node: &Rc<Node>) {
        let Some(content) = expander.child() else {
            return;
        };
        let (Some(icon), Some(name), Some(caption)) = (
            content.first_child().and_downcast::<gtk::Image>(),
            content
                .first_child()
                .and_then(|w| w.next_sibling())
                .and_downcast::<gtk::Label>(),
            content.last_child().and_downcast::<gtk::Label>(),
        ) else {
            return;
        };
        for class in ["sshdeck-db-active", "sshdeck-tree-note", "dim-label"] {
            name.remove_css_class(class);
        }
        icon.remove_css_class("sshdeck-tree-pk");
        icon.add_css_class("dim-label");
        icon.set_visible(true);
        let (icon_name, label, size, tooltip) = match &node.kind {
            NodeKind::Database(info) => {
                if *self.active_db.borrow() == info.name {
                    name.add_css_class("sshdeck-db-active");
                }
                self.db_labels
                    .borrow_mut()
                    .insert(info.name.clone(), name.downgrade());
                let mut tooltip = format!("База данных {}", info.name);
                if let Some(size) = info.size {
                    tooltip.push_str(&format!("\nРазмер: {}", format_size(size)));
                }
                tooltip.push_str(
                    "\nДвойной щелчок — сделать активной: запросы редактора пойдут в неё",
                );
                (
                    "drive-multidisk-symbolic",
                    info.name.clone(),
                    info.size.map(format_size),
                    tooltip,
                )
            }
            NodeKind::Schema {
                name: schema,
                size,
                tables,
                ..
            } => (
                "folder-symbolic",
                schema.clone(),
                size.map(format_size),
                format!(
                    "Схема {schema}: {}{}",
                    plural(*tables as u64, "таблица", "таблицы", "таблиц"),
                    size.map(|s| format!(", {}", format_size(s)))
                        .unwrap_or_default()
                ),
            ),
            NodeKind::Table { info, .. } => {
                let kind = if info.view {
                    "Представление"
                } else {
                    "Таблица"
                };
                let mut tooltip = format!("{kind} {}.{}", info.schema, info.name);
                if let Some(rows) = info.rows {
                    tooltip.push_str(&format!("\n≈ {} строк", group_digits(rows)));
                }
                if let Some(size) = info.size {
                    tooltip.push_str(&format!(
                        "\nРазмер вместе с индексами: {}",
                        format_size(size)
                    ));
                }
                tooltip.push_str("\nДвойной щелчок — данные, правая кнопка — меню");
                (
                    if info.view {
                        "view-reveal-symbolic"
                    } else {
                        "view-grid-symbolic"
                    },
                    info.name.clone(),
                    info.size.map(format_size),
                    tooltip,
                )
            }
            NodeKind::Column(column) => {
                if column.primary {
                    icon.add_css_class("sshdeck-tree-pk");
                    icon.remove_css_class("dim-label");
                }
                let mut tooltip = format!("{}: {}", column.name, column.type_name);
                if !column.nullable {
                    tooltip.push_str(", NOT NULL");
                }
                if column.primary {
                    tooltip.push_str(", первичный ключ");
                }
                (
                    if column.primary {
                        "dialog-password-symbolic"
                    } else {
                        "insert-text-symbolic"
                    },
                    column.name.clone(),
                    Some(column.type_name.clone()),
                    tooltip,
                )
            }
            NodeKind::Note(text) => {
                name.add_css_class("sshdeck-tree-note");
                name.add_css_class("dim-label");
                icon.set_visible(false);
                ("", text.clone(), None, text.clone())
            }
        };
        if !icon_name.is_empty() {
            icon.set_icon_name(Some(icon_name));
        }
        name.set_text(&label);
        caption.set_text(size.as_deref().unwrap_or(""));
        caption.set_visible(size.is_some());
        content.set_tooltip_text(Some(&tooltip));
    }

    // ----- соединение

    fn connect(self: &Rc<Self>) {
        // Прошлое соединение закрывается, его события больше не нужны.
        self.worker.take();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.connected.set(false);
        self.pending.set(None);
        self.transfer.take();
        self.banner.set_revealed(false);
        let tunnel = self.profile.is_networked() && !self.profile.ssh.is_empty();
        self.status.set_text(if tunnel {
            "Подключение через SSH…"
        } else {
            "Подключение…"
        });
        self.databases.borrow_mut().clear();
        self.catalogs.borrow_mut().clear();
        self.catalog_errors.borrow_mut().clear();
        self.requested_catalogs.borrow_mut().clear();
        self.columns.borrow_mut().clear();
        self.requested_columns.borrow_mut().clear();
        self.set_nav_note("Подключение…");
        self.rebuild_tree();
        self.set_busy(true);

        let control = if tunnel {
            crate::session::validate_target(&self.profile.ssh)
                .ok()
                .and_then(|target| (self.control)(&target))
        } else {
            None
        };
        let (tx, rx) = async_channel::unbounded();
        let request = ConnectRequest {
            profile: self.profile.clone(),
            password: self.password.borrow().clone(),
            control,
        };
        match DbWorker::start(request, tx) {
            Ok(worker) => *self.worker.borrow_mut() = Some(worker),
            Err(message) => {
                self.handle(Event::ConnectFailed(message));
                return;
            }
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                let Some(tab) = weak.upgrade() else { break };
                if tab.generation.get() != generation {
                    break;
                }
                tab.handle(event);
            }
        });
    }

    fn handle(&self, event: Event) {
        match event {
            Event::Connected {
                version,
                route,
                database,
            } => {
                self.connected.set(true);
                *self.status_base.borrow_mut() = format!("{version} · {route}");
                self.status
                    .set_tooltip_text(Some(&format!("{version}\n{route}")));
                // После переподключения активной остаётся выбранная база.
                if self.active_db.borrow().is_empty() || self.main_db.borrow().is_empty() {
                    *self.active_db.borrow_mut() = database.clone();
                }
                *self.main_db.borrow_mut() = database.clone();
                self.requested_catalogs
                    .borrow_mut()
                    .insert(database.clone());
                let active = self.active_db.borrow().clone();
                self.want_expanded
                    .borrow_mut()
                    .insert(format!("db\u{1}{active}"));
                if active != database {
                    self.request_catalog(&active);
                }
                self.update_status();
                self.set_nav_note("Загрузка списка таблиц…");
                self.results.show_hint(
                    "Напишите запрос и нажмите Ctrl+Enter.\n\
                     Двойной щелчок по таблице слева показывает её данные, \
                     правая кнопка — структуру, экспорт и импорт.",
                );
                self.set_busy(false);
            }
            Event::ConnectFailed(message) => {
                let auth = self.profile.is_networked() && db::is_auth_error(&message);
                self.auth_failed.set(auth);
                self.connected.set(false);
                self.status.set_text("Нет соединения");
                self.status.set_tooltip_text(None);
                if auth {
                    self.banner.set_title("Сервер не принял пароль");
                    self.banner.set_button_label(Some("Ввести пароль"));
                } else {
                    self.banner
                        .set_title("Не удалось подключиться к базе данных");
                    self.banner.set_button_label(Some("Переподключиться"));
                }
                self.banner.set_revealed(true);
                self.set_nav_note("Нет соединения");
                self.results.show_error("Подключение не удалось", &message);
                self.set_busy(false);
                if auth {
                    // Сохранённый пароль не подошёл — сразу просим новый.
                    self.ask_password();
                }
            }
            Event::Catalog { database, result } => self.catalog_loaded(database, result),
            Event::Databases(list) => {
                *self.databases.borrow_mut() = list;
                self.rebuild_tree();
            }
            Event::Columns {
                database,
                schema,
                table,
                result,
            } => {
                let key = (database, schema, table);
                self.requested_columns.borrow_mut().remove(&key);
                let result = result.map_err(|error| error.message);
                let children = column_nodes(&result);
                self.columns.borrow_mut().insert(key.clone(), result);
                let node = self.table_nodes.borrow().get(&key).cloned();
                if let Some(node) = node {
                    node.set_children(&children);
                }
            }
            Event::QueryDone {
                id,
                result,
                elapsed,
            } => {
                if self.pending.get() != Some(id) {
                    return;
                }
                self.pending.set(None);
                self.set_busy(false);
                match result {
                    Ok(output) => self.results.show_output(output, elapsed),
                    Err(error) => {
                        self.results.show_error("Ошибка", &error.message);
                        if error.lost {
                            self.lost();
                        }
                    }
                }
            }
            Event::Progress { id, rows } => {
                if self.pending.get() == Some(id) {
                    self.results.set_progress(rows);
                }
            }
            Event::TransferDone {
                id,
                result,
                elapsed,
            } => {
                if self.pending.get() != Some(id) {
                    return;
                }
                self.pending.set(None);
                self.set_busy(false);
                let Some(transfer) = self.transfer.take() else {
                    return;
                };
                match result {
                    Ok(report) => {
                        self.results.show_transfer(
                            &report,
                            transfer.import,
                            &transfer.target,
                            elapsed,
                        );
                        let rows = plural(report.rows, "строка", "строки", "строк");
                        if transfer.import {
                            self.notifier
                                .info(&format!("Загружено {rows} в {}", transfer.target));
                            // Размеры таблиц изменились.
                            self.request_catalog(&transfer.database);
                        } else {
                            self.notifier
                                .info(&format!("Выгружено {rows} в {}", file_name(&report.path)));
                        }
                    }
                    Err(error) => self.results.show_error(
                        if transfer.import {
                            "Импорт не удался, таблица не изменилась"
                        } else {
                            "Экспорт не удался"
                        },
                        &error.message,
                    ),
                }
            }
        }
    }

    /// Пришли схемы и таблицы базы.
    fn catalog_loaded(&self, database: String, result: Result<Catalog, db::DbError>) {
        self.requested_catalogs.borrow_mut().remove(&database);
        match result {
            Ok(catalog) => {
                // Единственная схема или public раскрывается сама.
                if !self.catalogs.borrow().contains_key(&database) {
                    let schema = if catalog.schemas.len() == 1 {
                        catalog.schemas.first().cloned()
                    } else {
                        catalog
                            .schemas
                            .iter()
                            .find(|s| *s == "public" || **s == self.profile.database)
                            .cloned()
                    };
                    if let Some(schema) = schema {
                        self.want_expanded
                            .borrow_mut()
                            .insert(format!("schema\u{1}{database}\u{1}{schema}"));
                    }
                }
                self.catalog_errors.borrow_mut().remove(&database);
                self.catalogs.borrow_mut().insert(database.clone(), catalog);
            }
            Err(error) => {
                self.catalog_errors
                    .borrow_mut()
                    .insert(database.clone(), error.message.clone());
                if error.lost {
                    self.lost();
                }
            }
        }
        if database == *self.main_db.borrow() {
            let empty = self
                .catalogs
                .borrow()
                .get(&database)
                .is_none_or(|c| c.tables.is_empty());
            let note = match self.catalog_errors.borrow().get(&database) {
                Some(error) => format!("Не удалось прочитать список таблиц:\n{error}"),
                None if empty => "В базе нет таблиц".to_string(),
                None => String::new(),
            };
            self.set_nav_note(&note);
        }
        // Узел базы уже в дереве — обновляются только его дети (дерево не
        // прыгает), иначе дерево строится заново.
        let node = self.db_nodes.borrow().get(&database).cloned();
        match node {
            Some(node) => {
                let mut expanded = self.expanded_keys();
                expanded.extend(self.want_expanded.borrow_mut().drain());
                let children = self.database_children(&database, &self.filter());
                node.set_children(&non_empty(children, "Нет схем"));
                self.expand_matching(&expanded, false);
            }
            None => self.rebuild_tree(),
        }
    }

    /// Спрашивает пароль заново и переподключается с ним.
    fn ask_password(&self) {
        let Some(tab) = self.me() else { return };
        let weak = Rc::downgrade(&tab);
        self.store.ask_password(
            &self.profile,
            Some("Сервер не принял пароль. Введите его ещё раз."),
            move |password| {
                if let Some(tab) = weak.upgrade() {
                    *tab.password.borrow_mut() = password;
                    tab.connect();
                }
            },
        );
    }

    /// Подсветка синтаксиса — один раз на серию правок.
    fn schedule_highlight(&self) {
        if self.highlight_scheduled.replace(true) {
            return;
        }
        let Some(tab) = self.me() else { return };
        let weak = Rc::downgrade(&tab);
        glib::idle_add_local_once(move || {
            if let Some(tab) = weak.upgrade() {
                tab.highlight_scheduled.set(false);
                tab.highlight();
            }
        });
    }

    fn highlight(&self) {
        let buffer = self.editor.buffer();
        let (start, end) = buffer.bounds();
        for tag in &self.syntax_tags {
            buffer.remove_tag(tag, &start, &end);
        }
        let text = buffer.text(&start, &end, false).to_string();
        // Смещения токенов байтовые, а буфер считает символы.
        let mut chars = 0i32;
        let mut bytes = 0usize;
        let mut to_chars = |byte: usize| {
            chars += char_count(&text[bytes..byte]);
            bytes = byte;
            chars
        };
        for (range, token) in db::sql::highlight(&text, self.profile.kind) {
            let from = to_chars(range.start);
            let to = to_chars(range.end);
            let tag = &self.syntax_tags[match token {
                db::sql::Token::Keyword => 0,
                db::sql::Token::String => 1,
                db::sql::Token::Number => 2,
                db::sql::Token::Comment => 3,
            }];
            buffer.apply_tag(
                tag,
                &buffer.iter_at_offset(from),
                &buffer.iter_at_offset(to),
            );
        }
    }

    fn lost(&self) {
        self.connected.set(false);
        self.status.set_text("Нет соединения");
        self.banner.set_title("Соединение с базой данных потеряно");
        self.banner.set_revealed(true);
        self.set_busy(false);
    }

    /// Крутилка и доступность кнопок: во время подключения и запроса.
    fn set_busy(&self, busy: bool) {
        self.spinner.set_visible(busy);
        self.run_button
            .set_sensitive(!busy && self.connected.get() && self.pending.get().is_none());
        self.stop_button.set_sensitive(self.pending.get().is_some());
    }

    fn update_status(&self) {
        let mut text = self.status_base.borrow().clone();
        let active = self.active_db.borrow();
        if !active.is_empty() {
            text.push_str(&format!(" · база {active}"));
        }
        self.status.set_text(&text);
    }

    /// Параметр `database` для запроса к базе `name`.
    fn db_param(&self, name: &str) -> Option<String> {
        (!name.is_empty() && *name != *self.main_db.borrow()).then(|| name.to_string())
    }

    // ----- запросы

    fn run_from_editor(&self, whole: bool) {
        let buffer = self.editor.buffer();
        let (start, end) = buffer.bounds();
        let text = buffer.text(&start, &end, false).to_string();
        let (sql, from, to) = if let Some((from, to)) = buffer.selection_bounds() {
            (
                buffer.text(&from, &to, false).to_string(),
                from.offset(),
                to.offset(),
            )
        } else if whole {
            (text.clone(), 0, end.offset())
        } else {
            let cursor = char_to_byte(&text, buffer.cursor_position());
            let Some(range) = db::sql::statement_at(&text, cursor, self.profile.kind) else {
                self.notifier
                    .info("Нечего выполнять: напишите запрос в редакторе");
                return;
            };
            (
                text[range.clone()].to_string(),
                char_count(&text[..range.start]),
                char_count(&text[..range.end]),
            )
        };
        if sql.trim().is_empty() {
            self.notifier
                .info("Нечего выполнять: напишите запрос в редакторе");
            return;
        }
        let database = self.active_db.borrow().clone();
        if self.execute_in(sql, &database, None) {
            buffer.remove_tag(&self.executed_tag, &start, &end);
            buffer.apply_tag(
                &self.executed_tag,
                &buffer.iter_at_offset(from),
                &buffer.iter_at_offset(to),
            );
        }
    }

    /// Отправляет запрос к базе `database`; `false`, если сейчас нельзя.
    /// `table` — просматриваемая таблица (имя для экспорта).
    fn execute_in(&self, sql: String, database: &str, table: Option<(String, String)>) -> bool {
        let caption = if database != *self.active_db.borrow() {
            format!("{database} · {sql}")
        } else {
            sql.clone()
        };
        let Some(id) = self.begin_request(&caption) else {
            return false;
        };
        let (table, stem) = table.unzip();
        *self.last_query.borrow_mut() = Some(LastQuery {
            sql: sql.clone(),
            database: database.to_string(),
            table,
            stem,
        });
        self.send(Request::Query {
            id,
            database: self.db_param(database),
            sql,
        });
        true
    }

    /// Готовит отправку: проверяет состояние, назначает номер и показывает
    /// подпись над результатом.
    fn begin_request(&self, caption: &str) -> Option<u64> {
        if !self.connected.get() {
            self.notifier.info("Нет соединения с базой данных");
            return None;
        }
        if self.pending.get().is_some() {
            self.notifier
                .info("Дождитесь окончания запроса или прервите его");
            return None;
        }
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.pending.set(Some(id));
        self.results.set_caption(caption);
        self.set_busy(true);
        Some(id)
    }

    fn send(&self, request: Request) {
        if let Some(worker) = self.worker.borrow().as_ref() {
            worker.send(request);
        }
    }

    fn cancel(&self) {
        if let Some(id) = self.pending.get() {
            self.send(Request::Cancel { id });
        }
    }

    fn qualified(&self, table: &TableInfo) -> String {
        db::sql::qualified(&table.schema, &table.name, self.profile.kind)
    }

    fn open_data(&self, database: &str, table: &TableInfo) {
        let sql =
            db::sql::select_preview(&table.schema, &table.name, self.profile.kind, PREVIEW_ROWS);
        self.execute_in(
            sql,
            database,
            Some((self.qualified(table), table.name.clone())),
        );
    }

    fn open_structure(&self, database: &str, table: &TableInfo) {
        let name = self.qualified(table);
        let Some(id) = self.begin_request(&format!("Структура {name}")) else {
            return;
        };
        self.last_query.take();
        self.send(Request::Structure {
            id,
            database: self.db_param(database),
            schema: table.schema.clone(),
            table: table.name.clone(),
        });
    }

    // ----- дерево

    fn filter(&self) -> String {
        self.search.text().trim().to_lowercase()
    }

    fn request_catalog(&self, database: &str) {
        if !self.connected.get() {
            return;
        }
        self.requested_catalogs
            .borrow_mut()
            .insert(database.to_string());
        self.send(Request::Catalog {
            database: self.db_param(database),
        });
    }

    /// Перечитывает загруженное: базы, их таблицы. Старые данные видны,
    /// пока не придут новые.
    fn refresh_tree(&self) {
        if !self.connected.get() {
            return;
        }
        let mut loaded: HashSet<String> = self.catalogs.borrow().keys().cloned().collect();
        loaded.extend(self.catalog_errors.borrow_mut().drain().map(|(db, _)| db));
        loaded.insert(self.main_db.borrow().clone());
        for database in loaded {
            self.request_catalog(&database);
        }
        self.columns.borrow_mut().clear();
        if self.profile.kind == DbKind::Postgres {
            self.send(Request::Databases);
        }
    }

    /// Подгружает детей раскрытого узла.
    fn ensure_loaded(&self, node: &Rc<Node>) {
        match &node.kind {
            NodeKind::Database(info) => {
                let known = self.catalogs.borrow().contains_key(&info.name)
                    || self.catalog_errors.borrow().contains_key(&info.name)
                    || self.requested_catalogs.borrow().contains(&info.name);
                if !known {
                    self.request_catalog(&info.name);
                }
            }
            NodeKind::Table { database, info } => {
                let key = (database.clone(), info.schema.clone(), info.name.clone());
                let known = self.columns.borrow().contains_key(&key)
                    || self.requested_columns.borrow().contains(&key);
                if !known && self.connected.get() {
                    self.requested_columns.borrow_mut().insert(key);
                    self.send(Request::Columns {
                        database: self.db_param(database),
                        schema: info.schema.clone(),
                        table: info.name.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    fn table_node(&self, database: &str, table: &TableInfo) -> Rc<Node> {
        let key = (
            database.to_string(),
            table.schema.clone(),
            table.name.clone(),
        );
        let children = match self.columns.borrow().get(&key) {
            Some(result) => column_nodes(result),
            None => vec![Node::note(LOADING)],
        };
        let node = Node::with_children(
            NodeKind::Table {
                database: database.to_string(),
                info: table.clone(),
            },
            &children,
        );
        self.table_nodes.borrow_mut().insert(key, Rc::clone(&node));
        node
    }

    /// Содержимое базы: схемы с таблицами (без уровня схем, если схема одна
    /// и это не PostgreSQL). Пока каталога нет — «Загрузка…» или ошибка.
    fn database_children(&self, database: &str, filter: &str) -> Vec<Rc<Node>> {
        let catalogs = self.catalogs.borrow();
        let Some(catalog) = catalogs.get(database) else {
            let note = match self.catalog_errors.borrow().get(database) {
                Some(error) => format!("Ошибка: {error}"),
                None => LOADING.to_string(),
            };
            return vec![Node::note(&note)];
        };
        let matches =
            |table: &TableInfo| filter.is_empty() || table.name.to_lowercase().contains(filter);
        if self.profile.kind != DbKind::Postgres && catalog.schemas.len() <= 1 {
            return catalog
                .tables
                .iter()
                .filter(|table| matches(table))
                .map(|table| self.table_node(database, table))
                .collect();
        }
        let mut nodes = Vec::new();
        for schema in &catalog.schemas {
            let tables: Vec<&TableInfo> = catalog
                .tables
                .iter()
                .filter(|table| table.schema == *schema)
                .collect();
            let shown: Vec<Rc<Node>> = tables
                .iter()
                .filter(|table| matches(table))
                .map(|table| self.table_node(database, table))
                .collect();
            if !filter.is_empty() && shown.is_empty() {
                continue;
            }
            let size = tables
                .iter()
                .filter_map(|table| table.size)
                .reduce(|a, b| a + b);
            nodes.push(Node::with_children(
                NodeKind::Schema {
                    database: database.to_string(),
                    name: schema.clone(),
                    size,
                    tables: tables.len(),
                },
                &non_empty(shown, "Нет таблиц"),
            ));
        }
        nodes
    }

    /// Строит дерево заново, сохраняя раскрытые узлы.
    fn rebuild_tree(&self) {
        let mut expanded = self.expanded_keys();
        expanded.extend(self.want_expanded.borrow_mut().drain());
        let filter = self.filter();
        self.db_nodes.borrow_mut().clear();
        self.table_nodes.borrow_mut().clear();
        self.db_labels.borrow_mut().clear();
        let databases = self.databases.borrow().clone();
        let items: Vec<Rc<Node>> = if databases.is_empty() {
            let main = self.main_db.borrow().clone();
            if self.catalogs.borrow().contains_key(&main) {
                self.database_children(&main, &filter)
            } else {
                Vec::new()
            }
        } else {
            databases
                .iter()
                .filter_map(|info| {
                    let loaded = self.catalogs.borrow().contains_key(&info.name);
                    let children = self.database_children(&info.name, &filter);
                    if !filter.is_empty() && (!loaded || children.is_empty()) {
                        return None;
                    }
                    let node = Node::with_children(
                        NodeKind::Database(info.clone()),
                        &non_empty(children, "Нет схем"),
                    );
                    self.db_nodes
                        .borrow_mut()
                        .insert(info.name.clone(), Rc::clone(&node));
                    Some(node)
                })
                .collect()
        };
        let boxed_items: Vec<glib::BoxedAnyObject> = items.iter().map(boxed).collect();
        self.root.splice(0, self.root.n_items(), &boxed_items);
        self.expand_matching(&expanded, !filter.is_empty());
        self.sync_placeholder();
    }

    fn expanded_keys(&self) -> HashSet<String> {
        (0..self.tree_model.n_items())
            .filter_map(|i| self.tree_model.row(i))
            .filter(gtk::TreeListRow::is_expanded)
            .filter_map(|row| row_node(&row))
            .map(|node| node.key())
            .collect()
    }

    /// Раскрывает узлы с ключами из `keys`; при поиске — все загруженные
    /// базы и схемы.
    fn expand_matching(&self, keys: &HashSet<String>, searching: bool) {
        let mut index = 0;
        while index < self.tree_model.n_items() {
            if let Some(row) = self.tree_model.row(index)
                && let Some(node) = row_node(&row)
            {
                let wanted = keys.contains(&node.key())
                    || (searching
                        && match &node.kind {
                            NodeKind::Database(info) => {
                                self.catalogs.borrow().contains_key(&info.name)
                            }
                            NodeKind::Schema { .. } => true,
                            _ => false,
                        });
                if wanted && row.is_expandable() && !row.is_expanded() {
                    row.set_expanded(true);
                }
            }
            index += 1;
        }
    }

    fn set_nav_note(&self, text: &str) {
        *self.nav_note.borrow_mut() = text.to_string();
        self.sync_placeholder();
    }

    /// Надпись видна, когда в дереве ничего нет.
    fn sync_placeholder(&self) {
        let empty = self.root.n_items() == 0;
        self.nav_placeholder.set_visible(empty);
        if !empty {
            return;
        }
        let searching = !self.filter().is_empty();
        let catalogs = self.catalogs.borrow();
        let has_tables = catalogs.values().any(|c| !c.tables.is_empty());
        // Нераскрытые базы ещё не прочитаны — в них поиск не заглядывает.
        let unloaded = self
            .databases
            .borrow()
            .iter()
            .any(|db| !catalogs.contains_key(&db.name));
        let text = if searching && unloaded {
            "Ничего не найдено.\nПоиск идёт по раскрытым базам — раскройте базу, \
             чтобы искать и в ней."
                .to_string()
        } else if searching && has_tables {
            "Ничего не найдено".to_string()
        } else {
            self.nav_note.borrow().clone()
        };
        self.nav_placeholder.set_text(&text);
    }

    fn activate_node(&self, row: &gtk::TreeListRow, node: &Rc<Node>) {
        match &node.kind {
            NodeKind::Database(info) => {
                self.set_active_database(&info.name);
                row.set_expanded(true);
            }
            NodeKind::Schema { .. } => row.set_expanded(!row.is_expanded()),
            NodeKind::Table { database, info } => self.open_data(database, info),
            NodeKind::Column(_) => {
                if let Some(name) = self.node_name(node) {
                    self.editor.buffer().insert_at_cursor(&name);
                    self.editor.grab_focus();
                }
            }
            NodeKind::Note(_) => {}
        }
    }

    /// Делает базу активной: в неё идут запросы редактора.
    fn set_active_database(&self, name: &str) {
        let old = self.active_db.replace(name.to_string());
        if old == name {
            return;
        }
        for (database, bold) in [(&old, false), (&name.to_string(), true)] {
            if let Some(label) = self
                .db_labels
                .borrow()
                .get(database)
                .and_then(glib::WeakRef::upgrade)
            {
                if bold {
                    label.add_css_class("sshdeck-db-active");
                } else {
                    label.remove_css_class("sshdeck-db-active");
                }
            }
        }
        self.update_status();
        self.notifier
            .info(&format!("Запросы редактора выполняются в базе {name}"));
    }

    /// Имя узла для вставки в редактор.
    fn node_name(&self, node: &Rc<Node>) -> Option<String> {
        let kind = self.profile.kind;
        match &node.kind {
            NodeKind::Database(info) => Some(info.name.clone()),
            NodeKind::Schema { name, .. } => Some(db::sql::quote_ident(name, kind)),
            NodeKind::Table { info, .. } => Some(self.qualified(info)),
            NodeKind::Column(column) => Some(db::sql::quote_ident(&column.name, kind)),
            NodeKind::Note(_) => None,
        }
    }

    fn node_menu(&self, node: &Rc<Node>) -> Option<gio::Menu> {
        let menu = gio::Menu::new();
        let section = |items: &[(&str, &str)]| {
            let section = gio::Menu::new();
            for (label, action) in items {
                section.append(Some(label), Some(action));
            }
            menu.append_section(None, &section);
        };
        match &node.kind {
            NodeKind::Database(info) => {
                if *self.active_db.borrow() != info.name {
                    section(&[("Сделать активной", "dbnav.activate")]);
                }
                section(&[
                    ("Обновить", "dbnav.refresh"),
                    ("Скопировать имя", "dbnav.copy-name"),
                ]);
            }
            NodeKind::Schema { .. } => section(&[
                ("Вставить имя в редактор", "dbnav.insert-name"),
                ("Скопировать имя", "dbnav.copy-name"),
                ("Обновить", "dbnav.refresh"),
            ]),
            NodeKind::Table { info, .. } => {
                let preview = format!("Данные (первые {PREVIEW_ROWS} строк)");
                section(&[(&preview, "dbnav.data"), ("Структура", "dbnav.structure")]);
                if info.view {
                    section(&[("Экспорт данных…", "dbnav.export")]);
                } else {
                    section(&[
                        ("Экспорт данных…", "dbnav.export"),
                        ("Импорт данных…", "dbnav.import"),
                    ]);
                }
                section(&[
                    ("Вставить имя в редактор", "dbnav.insert-name"),
                    ("Скопировать имя", "dbnav.copy-name"),
                ]);
            }
            NodeKind::Column(_) => section(&[
                ("Вставить имя в редактор", "dbnav.insert-name"),
                ("Скопировать имя", "dbnav.copy-name"),
            ]),
            NodeKind::Note(_) => return None,
        }
        Some(menu)
    }

    fn show_node_menu(&self, item: &gtk::ListItem, widget: &gtk::Widget, x: f64, y: f64) {
        let Some(node) = item
            .item()
            .and_downcast::<gtk::TreeListRow>()
            .and_then(|row| row_node(&row))
        else {
            return;
        };
        let Some(menu) = self.node_menu(&node) else {
            return;
        };
        if let Some(selection) = self.tree.model() {
            selection.select_item(item.position(), true);
        }
        *self.menu_node.borrow_mut() = Some(node);
        self.nav_menu.set_menu_model(Some(&menu));
        let (x, y) = widget
            .compute_point(
                &self.nav_scroll,
                &gtk::graphene::Point::new(x as f32, y as f32),
            )
            .map_or((x as i32, y as i32), |point| {
                (point.x() as i32, point.y() as i32)
            });
        self.nav_menu
            .set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, 1)));
        self.nav_menu.popup();
    }

    // ----- экспорт и импорт

    fn export_table(&self, database: &str, table: &TableInfo) {
        let name = self.qualified(table);
        let what = if table.view {
            "представления"
        } else {
            "таблицы"
        };
        let mut description = format!("Все строки {what} {}.{}", table.schema, table.name);
        if let Some(rows) = table.rows {
            description.push_str(&format!(" (≈ {})", group_digits(rows)));
        }
        self.ask_export(ExportSource {
            sql: format!("SELECT * FROM {name}"),
            database: database.to_string(),
            table: name,
            stem: table.name.clone(),
            description,
            local: None,
        });
    }

    /// Экспорт показанного результата. Если строк было больше лимита и
    /// запрос только читает, он выполняется заново и выгружается целиком.
    fn export_results(&self) {
        let output = Rc::clone(&self.results.current.borrow());
        if output.columns.is_empty() {
            self.notifier.info("Нечего выгружать");
            return;
        }
        let last = self.last_query.borrow().clone();
        let table = last
            .as_ref()
            .and_then(|last| last.table.clone())
            .unwrap_or_else(|| db::sql::quote_ident("result", self.profile.kind));
        let stem = last
            .as_ref()
            .and_then(|last| last.stem.clone())
            .unwrap_or_else(|| "result".to_string());
        let database = last
            .as_ref()
            .map_or_else(|| self.active_db.borrow().clone(), |l| l.database.clone());
        let rerun = output.truncated
            && last
                .as_ref()
                .is_some_and(|last| db::sql::is_read_only(&last.sql));
        let source = if let (true, Some(last)) = (rerun, last) {
            ExportSource {
                sql: last.sql,
                database,
                table,
                stem,
                description: format!(
                    "Запрос будет выполнен ещё раз, и в файл попадут все его строки, \
                     а не только первые {MAX_ROWS}."
                ),
                local: None,
            }
        } else {
            let description = if output.truncated {
                format!(
                    "В файл попадут только первые {MAX_ROWS} строк: запрос изменяет данные, \
                     поэтому повторно он не выполняется."
                )
            } else {
                format!(
                    "В файл попадут {} результата.",
                    plural(output.rows.len() as u64, "строка", "строки", "строк")
                )
            };
            ExportSource {
                sql: String::new(),
                database,
                table,
                stem,
                description,
                local: Some(output),
            }
        };
        self.ask_export(source);
    }

    fn ask_export(&self, source: ExportSource) {
        let labels: Vec<&str> = ExportFormat::ALL.iter().map(|f| f.label()).collect();
        let format = adw::ComboRow::builder()
            .title("Формат")
            .model(&gtk::StringList::new(&labels))
            .selected(self.export_format.get())
            .build();
        let fields = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .width_request(380)
            .build();
        fields.append(&format);
        let dialog = adw::AlertDialog::new(Some("Экспорт данных"), Some(&source.description));
        dialog.add_responses(&[(CANCEL, "Отмена"), (ACCEPT, "Сохранить как…")]);
        dialog.set_close_response(CANCEL);
        dialog.set_default_response(Some(ACCEPT));
        dialog.set_response_appearance(ACCEPT, adw::ResponseAppearance::Suggested);
        dialog.set_extra_child(Some(&fields));
        let weak = self.me.clone();
        dialog.choose(
            Some(self.widget()),
            gio::Cancellable::NONE,
            move |response| {
                let Some(tab) = weak.upgrade() else { return };
                if response != ACCEPT {
                    return;
                }
                let index = format.selected();
                tab.export_format.set(index);
                let format = ExportFormat::ALL
                    .get(index as usize)
                    .copied()
                    .unwrap_or(ExportFormat::Csv);
                tab.pick_export_file(source, format);
            },
        );
    }

    fn pick_export_file(&self, source: ExportSource, format: ExportFormat) {
        let dialog = gtk::FileDialog::builder()
            .title("Куда сохранить")
            .initial_name(format!(
                "{}.{}",
                file_stem(&source.stem),
                format.extension()
            ))
            .modal(true)
            .build();
        let weak = self.me.clone();
        dialog.save(
            super::parent_window(self.widget()).as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                let (Some(tab), Ok(file)) = (weak.upgrade(), result) else {
                    return;
                };
                if let Some(path) = file.path() {
                    tab.start_export(source, format, path);
                }
            },
        );
    }

    fn start_export(&self, source: ExportSource, format: ExportFormat, path: PathBuf) {
        if let Some(output) = &source.local {
            match write_local(output, &path, format, &source.table, self.profile.kind) {
                Ok(rows) => self.notifier.info(&format!(
                    "Сохранено {} в {}",
                    plural(rows, "строка", "строки", "строк"),
                    file_name(&path)
                )),
                Err(message) => self
                    .notifier
                    .error(&format!("Не удалось сохранить файл: {message}")),
            }
            return;
        }
        let Some(id) = self.begin_request(&format!("Экспорт → {}", path.display())) else {
            return;
        };
        *self.transfer.borrow_mut() = Some(Transfer {
            import: false,
            database: source.database.clone(),
            target: source.table.clone(),
        });
        self.send(Request::Export {
            id,
            database: self.db_param(&source.database),
            sql: source.sql,
            path,
            format,
            table: source.table,
        });
    }

    fn import_table(&self, database: &str, table: &TableInfo) {
        let data = gtk::FileFilter::new();
        data.set_name(Some("CSV, TSV, JSON"));
        for pattern in [
            "*.csv", "*.tsv", "*.txt", "*.json", "*.CSV", "*.TSV", "*.JSON",
        ] {
            data.add_pattern(pattern);
        }
        let all = gtk::FileFilter::new();
        all.set_name(Some("Все файлы"));
        all.add_pattern("*");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&data);
        filters.append(&all);
        let dialog = gtk::FileDialog::builder()
            .title(format!("Импорт в {}.{}", table.schema, table.name))
            .filters(&filters)
            .default_filter(&data)
            .modal(true)
            .build();
        let weak = self.me.clone();
        let database = database.to_string();
        let table = table.clone();
        dialog.open(
            super::parent_window(self.widget()).as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                let (Some(tab), Ok(file)) = (weak.upgrade(), result) else {
                    return;
                };
                if let Some(path) = file.path() {
                    tab.ask_import_options(database, table, path);
                }
            },
        );
    }

    fn ask_import_options(&self, database: String, table: TableInfo, path: PathBuf) {
        let json = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        let defaults = ImportOptions::default();
        let header = adw::SwitchRow::builder()
            .title("Первая строка — имена колонок")
            .subtitle("Иначе значения идут в колонки таблицы по порядку")
            .active(defaults.header)
            .visible(!json)
            .build();
        let nulls = adw::SwitchRow::builder()
            .title("Пустые значения — NULL")
            .subtitle("Иначе — пустые строки")
            .active(defaults.empty_as_null)
            .build();
        let truncate = adw::SwitchRow::builder()
            .title("Очистить таблицу перед загрузкой")
            .subtitle("Все строки таблицы будут удалены")
            .active(defaults.truncate)
            .build();
        let fields = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .width_request(400)
            .build();
        fields.append(&header);
        fields.append(&nulls);
        fields.append(&truncate);
        let target = format!("{}.{}", table.schema, table.name);
        let atomic = if self.profile.kind == DbKind::Extension {
            "Строки вставляются порциями: при ошибке уже вставленные останутся."
        } else {
            "Загрузка идёт одной транзакцией: при ошибке таблица не изменится."
        };
        let body = format!(
            "{} → {}.{}\n\nКолонки файла сопоставляются с колонками таблицы по именам. {atomic}",
            file_name(&path),
            table.schema,
            table.name
        );
        let dialog = adw::AlertDialog::new(Some("Импорт данных"), Some(&body));
        dialog.add_responses(&[(CANCEL, "Отмена"), (ACCEPT, "Загрузить")]);
        dialog.set_close_response(CANCEL);
        dialog.set_default_response(Some(ACCEPT));
        dialog.set_response_appearance(ACCEPT, adw::ResponseAppearance::Suggested);
        dialog.set_extra_child(Some(&fields));
        let weak = self.me.clone();
        dialog.choose(
            Some(self.widget()),
            gio::Cancellable::NONE,
            move |response| {
                let Some(tab) = weak.upgrade() else { return };
                if response != ACCEPT {
                    return;
                }
                let options = ImportOptions {
                    header: header.is_active() || json,
                    empty_as_null: nulls.is_active(),
                    truncate: truncate.is_active(),
                };
                let Some(id) =
                    tab.begin_request(&format!("Импорт {} → {target}", file_name(&path)))
                else {
                    return;
                };
                *tab.transfer.borrow_mut() = Some(Transfer {
                    import: true,
                    database: database.clone(),
                    target: target.clone(),
                });
                tab.send(Request::Import {
                    id,
                    database: tab.db_param(&database),
                    schema: table.schema.clone(),
                    table: table.name.clone(),
                    path: path.clone(),
                    options,
                });
            },
        );
    }
}

/// Дети узла таблицы: колонки, «Нет колонок» или ошибка.
fn column_nodes(result: &Result<Vec<ColumnInfo>, String>) -> Vec<Rc<Node>> {
    match result {
        Ok(columns) if columns.is_empty() => vec![Node::note("Нет колонок")],
        Ok(columns) => columns
            .iter()
            .map(|column| Node::new(NodeKind::Column(column.clone())))
            .collect(),
        Err(error) => vec![Node::note(&format!("Ошибка: {error}"))],
    }
}

/// У раскрываемого узла должен быть хотя бы один ребёнок.
fn non_empty(nodes: Vec<Rc<Node>>, note: &str) -> Vec<Rc<Node>> {
    if nodes.is_empty() {
        vec![Node::note(note)]
    } else {
        nodes
    }
}

/// Пишет уже полученные строки в файл.
fn write_local(
    output: &QueryOutput,
    path: &Path,
    format: ExportFormat,
    table: &str,
    kind: DbKind,
) -> Result<u64, String> {
    use db::transfer::{ExportColumn, ValueKind};
    let numeric = numeric_columns(output);
    let columns: Vec<ExportColumn> = output
        .columns
        .iter()
        .zip(numeric)
        .map(|(name, numeric)| ExportColumn {
            name: name.clone(),
            kind: if numeric {
                ValueKind::Number
            } else {
                ValueKind::Text
            },
        })
        .collect();
    let write = || -> std::io::Result<()> {
        let mut writer = db::transfer::create_writer(path, format, table, kind)?;
        writer.start(&columns)?;
        for row in &output.rows {
            writer.row(row)?;
        }
        writer.finish()
    };
    write().map_err(|err| err.to_string())?;
    Ok(output.rows.len() as u64)
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Имя файла из имени таблицы: без разделителей пути.
fn file_stem(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let stem = stem.trim_matches('.').trim();
    if stem.is_empty() {
        "export".to_string()
    } else {
        stem.to_string()
    }
}

/// `12345678` → `12 345 678` (неразрывные пробелы).
fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{a0}');
        }
        out.push(c);
    }
    out
}

/// Таблица результатов с подписью и плашками итогов.
struct Results {
    root: gtk::Box,
    caption: gtk::Label,
    /// Плашки над таблицей: «200 строк», «12 мс».
    rows_pill: gtk::Label,
    time_pill: gtk::Label,
    export_button: gtk::Button,
    stack: gtk::Stack,
    view: gtk::ColumnView,
    /// «Запрос не вернул строк» поверх пустой таблицы.
    empty_note: gtk::Label,
    hint: gtk::Label,
    /// Сообщение: ошибка или итог команды без строк.
    message_box: gtk::Box,
    message_icon: gtk::Image,
    message_title: gtk::Label,
    message: gtk::Label,
    /// Показанный результат: из него берутся значения ячеек.
    current: RefCell<Rc<QueryOutput>>,
}

impl Results {
    fn new() -> Rc<Self> {
        let caption = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["dim-label", "sshdeck-db-caption"])
            .build();
        let pill = || {
            gtk::Label::builder()
                .visible(false)
                .css_classes(["sshdeck-pill"])
                .build()
        };
        let rows_pill = pill();
        let time_pill = pill();
        let export_button = gtk::Button::builder()
            .icon_name("document-save-symbolic")
            .tooltip_text("Сохранить результат в файл (CSV, JSON, SQL)")
            .css_classes(["flat"])
            .visible(false)
            .build();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.add_css_class("sshdeck-db-toolbar");
        header.add_css_class("sshdeck-results-header");
        header.append(&caption);
        header.append(&rows_pill);
        header.append(&time_pill);
        header.append(&export_button);

        let view = gtk::ColumnView::builder()
            .show_row_separators(false)
            .show_column_separators(true)
            .reorderable(false)
            .css_classes(["sshdeck-grid"])
            .build();
        let grid_scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hexpand(true)
            .build();
        let empty_note = gtk::Label::builder()
            .label("Запрос не вернул строк")
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .visible(false)
            .can_target(false)
            .css_classes(["dim-label"])
            .build();
        let grid = gtk::Overlay::builder().child(&grid_scroll).build();
        grid.add_overlay(&empty_note);

        let hint = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .valign(gtk::Align::Center)
            .css_classes(["dim-label"])
            .build();

        let message_icon = gtk::Image::builder()
            .pixel_size(20)
            .valign(gtk::Align::Start)
            .build();
        let message_title = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["heading"])
            .build();
        let message = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .selectable(true)
            .xalign(0.0)
            .css_classes(["sshdeck-message-text"])
            .build();
        let message_texts = gtk::Box::new(gtk::Orientation::Vertical, 4);
        message_texts.set_hexpand(true);
        message_texts.append(&message_title);
        message_texts.append(&message);
        let message_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .valign(gtk::Align::Start)
            .margin_top(14)
            .margin_bottom(14)
            .margin_start(14)
            .margin_end(14)
            .css_classes(["sshdeck-message"])
            .build();
        message_box.append(&message_icon);
        message_box.append(&message_texts);
        let message_scroll = gtk::ScrolledWindow::builder()
            .child(&message_box)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(ANIMATION_MS)
            .vexpand(true)
            .build();
        stack.add_named(&hint, Some("hint"));
        stack.add_named(&grid, Some("grid"));
        stack.add_named(&message_scroll, Some("message"));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&header);
        root.append(&stack);

        let results = Rc::new(Self {
            root,
            caption,
            rows_pill,
            time_pill,
            export_button,
            stack,
            view,
            empty_note,
            hint,
            message_box,
            message_icon,
            message_title,
            message,
            current: RefCell::new(Rc::new(QueryOutput::default())),
        });
        {
            // Ctrl+C — выделенные строки через табуляцию.
            let keys = gtk::EventControllerKey::new();
            let weak = Rc::downgrade(&results);
            keys.connect_key_pressed(move |_, key, _, modifiers| {
                let copy = matches!(key, gdk::Key::c | gdk::Key::C | gdk::Key::Insert)
                    && modifiers.contains(gdk::ModifierType::CONTROL_MASK);
                match weak.upgrade() {
                    Some(results) if copy => {
                        results.copy_selection();
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            });
            results.view.add_controller(keys);
        }
        results.show_hint("Подключение…");
        results
    }

    fn set_pills(&self, rows: Option<String>, time: Option<String>) {
        for (pill, text) in [(&self.rows_pill, rows), (&self.time_pill, time)] {
            pill.set_visible(text.is_some());
            pill.set_text(text.as_deref().unwrap_or(""));
        }
    }

    fn set_caption(&self, sql: &str) {
        let line = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        self.caption.set_text(&line);
        self.caption.set_tooltip_text(Some(sql));
        self.set_pills(None, Some("выполняется…".to_string()));
    }

    /// Сколько строк уже загружено при импорте.
    fn set_progress(&self, rows: u64) {
        self.set_pills(
            Some(plural(rows, "строка", "строки", "строк")),
            Some("выполняется…".to_string()),
        );
    }

    fn show_hint(&self, text: &str) {
        self.hint.set_text(text);
        self.set_pills(None, None);
        self.export_button.set_visible(false);
        self.stack.set_visible_child_name("hint");
    }

    fn show_message(&self, error: bool, title: &str, text: &str) {
        if error {
            self.message_box.add_css_class("error");
            self.message_box.remove_css_class("success");
            self.message_icon
                .set_icon_name(Some("dialog-error-symbolic"));
        } else {
            self.message_box.add_css_class("success");
            self.message_box.remove_css_class("error");
            self.message_icon
                .set_icon_name(Some("object-select-symbolic"));
        }
        self.message_title.set_text(title);
        self.message.set_text(text);
        self.message.set_visible(!text.is_empty());
        self.export_button.set_visible(false);
        self.stack.set_visible_child_name("message");
    }

    fn show_error(&self, heading: &str, message: &str) {
        self.set_pills(None, None);
        self.show_message(true, heading, message);
    }

    fn show_transfer(
        &self,
        report: &TransferReport,
        import: bool,
        target: &str,
        elapsed: Duration,
    ) {
        let rows = plural(report.rows, "строка", "строки", "строк");
        let title = if import {
            format!("Загружено {rows} в {target}")
        } else {
            format!("Выгружено {rows}")
        };
        let mut text = if import {
            format!("Из файла {}", report.path.display())
        } else {
            format!("Файл: {}", report.path.display())
        };
        if !report.skipped.is_empty() {
            text.push_str(&format!(
                "\nПропущены колонки файла, которых нет в таблице: {}",
                report.skipped.join(", ")
            ));
        }
        if let Some(note) = &report.note {
            text.push('\n');
            text.push_str(note);
        }
        self.set_pills(Some(rows.clone()), Some(format_elapsed(elapsed)));
        self.show_message(false, &title, &text);
        *self.current.borrow_mut() = Rc::new(QueryOutput::default());
    }

    fn show_output(&self, output: QueryOutput, elapsed: Duration) {
        let time = format_elapsed(elapsed);
        if output.columns.is_empty() {
            let title = match output.affected {
                Some(rows) => format!(
                    "Готово: затронуто {}",
                    plural(rows, "строка", "строки", "строк")
                ),
                None => "Готово".to_string(),
            };
            let detail = if output.statements > 1 {
                format!("Выполнено запросов: {}", output.statements)
            } else {
                String::new()
            };
            self.set_pills(None, Some(time));
            self.show_message(false, &title, &detail);
            *self.current.borrow_mut() = Rc::new(QueryOutput::default());
            return;
        }

        let rows = if output.truncated {
            format!("первые {MAX_ROWS} строк")
        } else {
            plural(output.rows.len() as u64, "строка", "строки", "строк")
        };
        self.set_pills(Some(rows), Some(time));

        let output = Rc::new(output);
        *self.current.borrow_mut() = Rc::clone(&output);
        while let Some(column) = self
            .view
            .columns()
            .item(0)
            .and_downcast::<gtk::ColumnViewColumn>()
        {
            self.view.remove_column(&column);
        }
        self.view.append_column(&row_number_column());
        let numeric = numeric_columns(&output);
        for (index, name) in output.columns.iter().enumerate() {
            let column = value_column(&output, index, numeric[index]);
            column.set_title(Some(name));
            self.view.append_column(&column);
        }
        // Пустая колонка забирает остаток ширины: полосы строк идут до
        // края, а последняя колонка не растягивается.
        self.view.append_column(&filler_column());
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let items: Vec<glib::BoxedAnyObject> = (0..output.rows.len())
            .map(glib::BoxedAnyObject::new)
            .collect();
        store.extend_from_slice(&items);
        self.view
            .set_model(Some(&gtk::MultiSelection::new(Some(store))));
        self.empty_note.set_visible(output.rows.is_empty());
        self.export_button.set_visible(true);
        self.stack.set_visible_child_name("grid");
    }

    /// Номера выделенных строк по порядку.
    fn selected_rows(&self) -> Vec<usize> {
        let Some(model) = self.view.model() else {
            return Vec::new();
        };
        let selection = model.selection();
        (0..selection.size())
            .map(|n| selection.nth(n as u32) as usize)
            .collect()
    }

    fn copy_selection(&self) {
        let output = Rc::clone(&self.current.borrow());
        let lines: Vec<String> = self
            .selected_rows()
            .into_iter()
            .filter_map(|row| output.rows.get(row))
            .map(|values| {
                values
                    .iter()
                    .map(|value| value.as_deref().unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect();
        if !lines.is_empty() {
            self.view.clipboard().set_text(&lines.join("\n"));
        }
    }

    /// Строка целиком: имя колонки и полное значение.
    fn show_row(&self, position: u32) {
        let output = Rc::clone(&self.current.borrow());
        let Some(values) = output.rows.get(position as usize) else {
            return;
        };
        let text = output
            .columns
            .iter()
            .zip(values)
            .map(|(name, value)| format!("{name}: {}", value.as_deref().unwrap_or("NULL")))
            .collect::<Vec<_>>()
            .join("\n");
        let view = gtk::TextView::builder()
            .editable(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .build();
        view.buffer().set_text(&text);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .min_content_height(320)
            .min_content_width(520)
            .css_classes(["card"])
            .build();
        let dialog = adw::AlertDialog::new(Some(&format!("Строка {}", position + 1)), None);
        dialog.add_responses(&[("copy", "Скопировать"), ("close", "Закрыть")]);
        dialog.set_close_response("close");
        dialog.set_default_response(Some("close"));
        dialog.set_extra_child(Some(&scroll));
        let clipboard = self.view.clipboard();
        dialog.choose(Some(&self.view), gio::Cancellable::NONE, move |response| {
            if response == "copy" {
                clipboard.set_text(&text);
            }
        });
    }
}

/// Номер строки в данных из элемента модели.
fn row_index(item: &gtk::ListItem) -> Option<usize> {
    item.item()
        .and_downcast::<glib::BoxedAnyObject>()
        .map(|row| *row.borrow::<usize>())
}

/// Ячейка-надпись; фон чередуется по номеру строки (CSS `:nth-child` не
/// годится: список переиспользует виджеты).
fn cell_label(item: &gtk::ListItem, numeric: bool) -> gtk::Label {
    let label = gtk::Label::builder()
        .xalign(if numeric { 1.0 } else { 0.0 })
        .hexpand(true)
        .ellipsize(pango::EllipsizeMode::End)
        .max_width_chars(40)
        .single_line_mode(true)
        .css_classes(["sshdeck-cell"])
        .build();
    if numeric {
        label.add_css_class("numeric");
    }
    item.set_child(Some(&label));
    label
}

fn set_stripe(label: &gtk::Label, row: usize) {
    if row % 2 == 1 {
        label.add_css_class("odd");
    } else {
        label.remove_css_class("odd");
    }
}

/// Колонка с номерами строк слева.
fn row_number_column() -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            let label = cell_label(item, true);
            label.add_css_class("sshdeck-rownum");
        }
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(label), Some(row)) = (item.child().and_downcast::<gtk::Label>(), row_index(item))
        else {
            return;
        };
        label.set_text(&(row + 1).to_string());
        set_stripe(&label, row);
    });
    gtk::ColumnViewColumn::builder()
        .title("#")
        .factory(&factory)
        .build()
}

/// Пустая растягивающаяся колонка справа.
fn filler_column() -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            // Пробел, а не пустая строка: у пустой надписи нет высоты строки.
            cell_label(item, false).set_text(" ");
        }
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        if let (Some(label), Some(row)) =
            (item.child().and_downcast::<gtk::Label>(), row_index(item))
        {
            set_stripe(&label, row);
        }
    });
    gtk::ColumnViewColumn::builder()
        .factory(&factory)
        .expand(true)
        .build()
}

/// Колонка значений `index`; числа выравниваются вправо.
fn value_column(output: &Rc<QueryOutput>, index: usize, numeric: bool) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            cell_label(item, numeric);
        }
    });
    let data = Rc::clone(output);
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(label), Some(row)) = (item.child().and_downcast::<gtk::Label>(), row_index(item))
        else {
            return;
        };
        set_stripe(&label, row);
        match data.rows.get(row).and_then(|values| values.get(index)) {
            Some(Some(text)) => {
                label.set_text(&cell_preview(text));
                label.remove_css_class("sshdeck-null");
            }
            _ => {
                label.set_text("NULL");
                label.add_css_class("sshdeck-null");
            }
        }
    });
    gtk::ColumnViewColumn::builder()
        .factory(&factory)
        .resizable(true)
        .build()
}

/// Какие колонки числовые: все непустые значения — числа (и хотя бы одно
/// есть).
fn numeric_columns(output: &QueryOutput) -> Vec<bool> {
    (0..output.columns.len())
        .map(|index| {
            let mut values = output
                .rows
                .iter()
                .filter_map(|row| row.get(index).and_then(Option::as_deref))
                .peekable();
            values.peek().is_some() && values.all(|value| value.trim().parse::<f64>().is_ok())
        })
        .collect()
}

/// Значение для ячейки: одной строкой и не слишком длинное.
fn cell_preview(text: &str) -> String {
    let mut preview: String = text
        .chars()
        .take(CELL_PREVIEW_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if text.chars().nth(CELL_PREVIEW_CHARS).is_some() {
        preview.push('…');
    }
    preview
}

/// `1 строка`, `3 строки`, `11 строк`.
fn plural(n: u64, one: &str, few: &str, many: &str) -> String {
    let word = match (n % 10, n % 100) {
        (1, rest) if rest != 11 => one,
        (2..=4, rest) if !(12..=14).contains(&rest) => few,
        _ => many,
    };
    format!("{} {word}", group_digits(n))
}

/// `12 мс`, `1,4 с`.
fn format_elapsed(elapsed: Duration) -> String {
    let ms = elapsed.as_millis();
    if ms < 1000 {
        format!("{ms} мс")
    } else {
        format!("{:.1} с", elapsed.as_secs_f64()).replace('.', ",")
    }
}

/// Смещение в символах (как в GTK) → смещение в байтах.
fn char_to_byte(text: &str, chars: i32) -> usize {
    let chars = usize::try_from(chars).unwrap_or(0);
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(byte, _)| byte)
}

fn char_count(text: &str) -> i32 {
    i32::try_from(text.chars().count()).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_detection() {
        let output = QueryOutput {
            columns: vec!["id".into(), "name".into(), "price".into(), "empty".into()],
            rows: vec![
                vec![
                    Some("1".into()),
                    Some("a".into()),
                    Some("12.50".into()),
                    None,
                ],
                vec![Some("2".into()), Some("7".into()), None, None],
            ],
            ..QueryOutput::default()
        };
        assert_eq!(numeric_columns(&output), [true, false, true, false]);
    }

    #[test]
    fn plurals() {
        assert_eq!(plural(1, "строка", "строки", "строк"), "1 строка");
        assert_eq!(plural(3, "строка", "строки", "строк"), "3 строки");
        assert_eq!(plural(5, "строка", "строки", "строк"), "5 строк");
        assert_eq!(plural(11, "строка", "строки", "строк"), "11 строк");
        assert_eq!(plural(12, "строка", "строки", "строк"), "12 строк");
        assert_eq!(plural(21, "строка", "строки", "строк"), "21 строка");
        assert_eq!(plural(104, "строка", "строки", "строк"), "104 строки");
        assert_eq!(plural(0, "строка", "строки", "строк"), "0 строк");
        assert_eq!(
            plural(12_341, "строка", "строки", "строк"),
            "12\u{a0}341 строка"
        );
    }

    #[test]
    fn digit_groups() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1\u{a0}000");
        assert_eq!(group_digits(12_345_678), "12\u{a0}345\u{a0}678");
    }

    #[test]
    fn file_stems() {
        assert_eq!(file_stem("orders"), "orders");
        assert_eq!(file_stem("a/b"), "a_b");
        assert_eq!(file_stem(".."), "export");
    }

    #[test]
    fn elapsed() {
        assert_eq!(format_elapsed(Duration::from_millis(12)), "12 мс");
        assert_eq!(format_elapsed(Duration::from_millis(1440)), "1,4 с");
    }

    #[test]
    fn previews() {
        assert_eq!(cell_preview("a\nb\tc"), "a b c");
        let long = "я".repeat(CELL_PREVIEW_CHARS + 5);
        let preview = cell_preview(&long);
        assert_eq!(preview.chars().count(), CELL_PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn offsets() {
        let text = "привет;x";
        assert_eq!(char_to_byte(text, 0), 0);
        assert_eq!(char_to_byte(text, 6), "привет".len());
        assert_eq!(char_to_byte(text, 100), text.len());
        assert_eq!(char_count("привет"), 6);
    }
}
