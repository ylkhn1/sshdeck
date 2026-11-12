//! Вкладка базы данных: слева навигатор таблиц, справа SQL-редактор и
//! таблица результатов.
//!
//! Ctrl+Enter выполняет запрос под курсором (или выделенный текст),
//! Ctrl+Shift+Enter — весь текст редактора. Щелчок по таблице показывает её
//! первые строки, правая кнопка — структуру.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};

use super::databases::{Databases, kind_badge};
use super::{ANIMATION_MS, Notifier};
use crate::db::{
    self, ConnectRequest, DbProfile, DbWorker, Event, MAX_ROWS, QueryOutput, Request, TableInfo,
};

/// Сколько строк показывать при щелчке по таблице.
const PREVIEW_ROWS: usize = 200;
/// Длиннее этого значение в ячейке обрезается (целиком оно видно по
/// двойному щелчку на строке).
const CELL_PREVIEW_CHARS: usize = 200;
const NAV_WIDTH: i32 = 250;
const EDITOR_HEIGHT: i32 = 200;

/// Ищет сокет мастер-соединения открытой вкладки к SSH-серверу (адрес уже
/// проверен `validate_target`).
pub type ControlLookup = Rc<dyn Fn(&str) -> Option<PathBuf>>;

#[derive(Clone)]
enum NavRow {
    Schema(String),
    Table(TableInfo),
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
    /// Запрос, результата которого ждём.
    pending: Cell<Option<u64>>,
    next_id: Cell<u64>,

    banner: adw::Banner,
    status: gtk::Label,
    spinner: adw::Spinner,
    run_button: gtk::Button,
    stop_button: gtk::Button,
    editor: gtk::TextView,
    executed_tag: gtk::TextTag,
    /// Теги подсветки SQL: ключевые слова, строки, числа, комментарии.
    syntax_tags: [gtk::TextTag; 4],
    /// Подсветка уже запланирована на ближайший простой.
    highlight_scheduled: Cell<bool>,

    /// Выбор базы на сервере (PostgreSQL); строка скрыта, пока список баз
    /// не пришёл.
    database_row: gtk::Box,
    database_choice: gtk::DropDown,
    /// Базы на сервере в порядке выпадающего списка.
    databases: RefCell<Vec<String>>,
    /// База, к которой подключаемся (сначала — из профиля).
    database: RefCell<String>,
    /// Список меняется программно: выбор в нём не означает переключения.
    syncing_databases: Cell<bool>,

    search: gtk::SearchEntry,
    nav: gtk::ListBox,
    /// Надпись вместо пустого списка (своя, а не заглушка `ListBox`:
    /// ту `remove_all` удаляет вместе со строками).
    nav_placeholder: gtk::Label,
    /// Что писать, когда таблиц нет: «Подключение…», «В базе нет таблиц»…
    nav_note: RefCell<String>,
    nav_rows: RefCell<Vec<NavRow>>,
    tables: RefCell<Vec<TableInfo>>,
    collapsed: RefCell<BTreeSet<String>>,
    nav_menu: gtk::PopoverMenu,
    menu_table: RefCell<Option<TableInfo>>,

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

        // --- навигатор
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Поиск таблиц")
            .hexpand(true)
            .build();
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Перечитать список таблиц")
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
        let nav = gtk::ListBox::builder()
            .css_classes(["navigation-sidebar", "sshdeck-db-nav"])
            .build();
        let nav_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        nav_content.append(&nav);
        nav_content.append(&nav_placeholder);
        let nav_scroll = gtk::ScrolledWindow::builder()
            .child(&nav_content)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let database_choice = gtk::DropDown::builder()
            .model(&gtk::StringList::new(&[]))
            .hexpand(true)
            .tooltip_text("База данных на сервере")
            .build();
        let database_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        database_row.add_css_class("sshdeck-db-toolbar");
        let database_icon = gtk::Image::from_icon_name("drive-multidisk-symbolic");
        database_icon.add_css_class("dim-label");
        database_icon.set_margin_start(4);
        database_row.append(&database_icon);
        database_row.append(&database_choice);
        database_row.set_visible(false);
        let nav_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        nav_box.append(&database_row);
        nav_box.append(&nav_header);
        nav_box.append(&nav_scroll);

        let menu = gio::Menu::new();
        let open_section = gio::Menu::new();
        open_section.append(
            Some(&format!("Данные (первые {PREVIEW_ROWS} строк)")),
            Some("dbnav.data"),
        );
        open_section.append(Some("Структура"), Some("dbnav.structure"));
        menu.append_section(None, &open_section);
        let name_section = gio::Menu::new();
        name_section.append(Some("Вставить имя в редактор"), Some("dbnav.insert-name"));
        name_section.append(Some("Скопировать имя"), Some("dbnav.copy-name"));
        menu.append_section(None, &name_section);
        let nav_menu = gtk::PopoverMenu::from_model(Some(&menu));
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
            .tooltip_text("Прервать запрос")
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
        toolbar.append(&kind_badge(profile.kind));
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

        let profile_database = profile.database.clone();
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
            banner,
            status,
            spinner,
            run_button,
            stop_button,
            editor,
            executed_tag,
            syntax_tags,
            highlight_scheduled: Cell::new(false),
            database_row,
            database_choice,
            databases: RefCell::new(Vec::new()),
            database: RefCell::new(profile_database),
            syncing_databases: Cell::new(false),
            search,
            nav,
            nav_placeholder,
            nav_note: RefCell::new("Подключение…".to_string()),
            nav_rows: RefCell::new(Vec::new()),
            tables: RefCell::new(Vec::new()),
            collapsed: RefCell::new(BTreeSet::new()),
            nav_menu,
            menu_table: RefCell::new(None),
            results,
        });
        tab.connect_signals(&content, &refresh, &reconnect);
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
            let reload = with_tab(|tab| tab.reload_tables());
            refresh.connect_clicked(move |_| reload());
        }
        {
            let rebuild = with_tab(|tab| tab.rebuild_nav());
            self.search.connect_search_changed(move |_| rebuild());
        }
        {
            let switch = with_tab(|tab| tab.switch_database());
            self.database_choice
                .connect_selected_notify(move |_| switch());
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
            let weak = Rc::downgrade(self);
            self.nav.connect_row_activated(move |_, row| {
                let Some(tab) = weak.upgrade() else { return };
                let entry = usize::try_from(row.index())
                    .ok()
                    .and_then(|index| tab.nav_rows.borrow().get(index).cloned());
                match entry {
                    Some(NavRow::Schema(schema)) => tab.toggle_schema(&schema),
                    Some(NavRow::Table(table)) => tab.open_data(&table),
                    None => {}
                }
            });
        }
        {
            let click = gtk::GestureClick::builder()
                .button(gdk::BUTTON_SECONDARY)
                .build();
            let weak = Rc::downgrade(self);
            click.connect_pressed(move |_, _, x, y| {
                if let Some(tab) = weak.upgrade() {
                    tab.show_nav_menu(x, y);
                }
            });
            self.nav.add_controller(click);
        }
        {
            let actions = gio::SimpleActionGroup::new();
            let add = |name: &str, action: fn(&Self, &TableInfo)| {
                let weak = Rc::downgrade(self);
                let entry = gio::SimpleAction::new(name, None);
                entry.connect_activate(move |_, _| {
                    let Some(tab) = weak.upgrade() else { return };
                    let table = tab.menu_table.borrow().clone();
                    if let Some(table) = table {
                        action(&tab, &table);
                    }
                });
                actions.add_action(&entry);
            };
            add("data", |tab, table| tab.open_data(table));
            add("structure", |tab, table| tab.open_structure(table));
            add("insert-name", |tab, table| {
                let name = db::sql::qualified(&table.schema, &table.name, tab.profile.kind);
                tab.editor.buffer().insert_at_cursor(&name);
                tab.editor.grab_focus();
            });
            add("copy-name", |tab, table| {
                let name = db::sql::qualified(&table.schema, &table.name, tab.profile.kind);
                tab.editor.clipboard().set_text(&name);
                tab.notifier.info(&format!("Скопировано: {name}"));
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

    // ----- соединение

    fn connect(self: &Rc<Self>) {
        // Прошлое соединение закрывается, его события больше не нужны.
        self.worker.take();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.connected.set(false);
        self.pending.set(None);
        self.banner.set_revealed(false);
        let tunnel = self.profile.kind.is_networked() && !self.profile.ssh.is_empty();
        self.status.set_text(if tunnel {
            "Подключение через SSH…"
        } else {
            "Подключение…"
        });
        self.tables.borrow_mut().clear();
        self.rebuild_nav();
        self.set_nav_note("Подключение…");
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
            profile: DbProfile {
                database: self.database.borrow().clone(),
                ..self.profile.clone()
            },
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
            Event::Connected { version, route } => {
                self.connected.set(true);
                self.status.set_text(&format!("{version} · {route}"));
                self.status
                    .set_tooltip_text(Some(&format!("{version}\n{route}")));
                self.set_nav_note("Загрузка списка таблиц…");
                self.results.show_hint(
                    "Напишите запрос и нажмите Ctrl+Enter.\n\
                     Щелчок по таблице слева показывает её данные, правая кнопка — структуру.",
                );
                self.set_busy(false);
            }
            Event::ConnectFailed(message) => {
                let auth = self.profile.kind.is_networked() && db::is_auth_error(&message);
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
            Event::Tables(Ok(tables)) => {
                *self.tables.borrow_mut() = tables;
                let database = self.database.borrow().clone();
                self.set_nav_note(&if database.is_empty() {
                    "В базе нет таблиц".to_string()
                } else if self.databases.borrow().len() > 1 {
                    format!("В базе «{database}» нет таблиц.\nДругую базу можно выбрать вверху.")
                } else {
                    format!("В базе «{database}» нет таблиц")
                });
                self.rebuild_nav();
            }
            Event::Databases { names, current } => {
                self.show_databases(names, current);
            }
            Event::Tables(Err(error)) => {
                self.set_nav_note(&format!(
                    "Не удалось прочитать список таблиц:\n{}",
                    error.message
                ));
                self.tables.borrow_mut().clear();
                self.rebuild_nav();
                if error.lost {
                    self.lost();
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
        if self.execute(sql) {
            buffer.remove_tag(&self.executed_tag, &start, &end);
            buffer.apply_tag(
                &self.executed_tag,
                &buffer.iter_at_offset(from),
                &buffer.iter_at_offset(to),
            );
        }
    }

    /// Отправляет запрос; `false`, если сейчас нельзя.
    fn execute(&self, sql: String) -> bool {
        let Some(id) = self.begin_request(&sql) else {
            return false;
        };
        self.send(Request::Query { id, sql });
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
        if self.pending.get().is_some() {
            self.send(Request::Cancel);
        }
    }

    fn open_data(&self, table: &TableInfo) {
        let sql =
            db::sql::select_preview(&table.schema, &table.name, self.profile.kind, PREVIEW_ROWS);
        self.execute(sql);
    }

    fn open_structure(&self, table: &TableInfo) {
        let name = db::sql::qualified(&table.schema, &table.name, self.profile.kind);
        let Some(id) = self.begin_request(&format!("Структура {name}")) else {
            return;
        };
        self.send(Request::Structure {
            id,
            schema: table.schema.clone(),
            table: table.name.clone(),
        });
    }

    fn reload_tables(&self) {
        if self.connected.get() {
            self.set_nav_note("Загрузка списка таблиц…");
            self.send(Request::Tables);
        }
    }

    /// Заполняет выбор базы, не вызывая переключения.
    fn show_databases(&self, names: Vec<String>, current: String) {
        self.syncing_databases.set(true);
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        self.database_choice
            .set_model(Some(&gtk::StringList::new(&refs)));
        if let Some(position) = names.iter().position(|name| *name == current) {
            self.database_choice.set_selected(position as u32);
        }
        self.syncing_databases.set(false);
        self.database_row.set_visible(names.len() > 1);
        *self.databases.borrow_mut() = names;
        if !current.is_empty() {
            *self.database.borrow_mut() = current;
        }
        // Надпись о пустой базе могла прийти раньше списка баз.
        if self.tables.borrow().is_empty() && self.connected.get() {
            let database = self.database.borrow().clone();
            if self.databases.borrow().len() > 1 {
                self.set_nav_note(&format!(
                    "В базе «{database}» нет таблиц.\nДругую базу можно выбрать вверху."
                ));
            }
        }
    }

    /// Пользователь выбрал другую базу: переподключаемся к ней.
    fn switch_database(self: &Rc<Self>) {
        if self.syncing_databases.get() {
            return;
        }
        let selected = self.database_choice.selected() as usize;
        let Some(name) = self.databases.borrow().get(selected).cloned() else {
            return;
        };
        if *self.database.borrow() == name {
            return;
        }
        *self.database.borrow_mut() = name;
        self.connect();
    }

    // ----- навигатор

    fn toggle_schema(&self, schema: &str) {
        {
            let mut collapsed = self.collapsed.borrow_mut();
            if !collapsed.remove(schema) {
                collapsed.insert(schema.to_string());
            }
        }
        self.rebuild_nav();
    }

    fn rebuild_nav(&self) {
        // `remove_all`, а не обход детей: заглушка пустого списка тоже
        // ребёнок, и её удалять нельзя.
        self.nav.remove_all();
        let filter = self.search.text().trim().to_lowercase();
        let tables = self.tables.borrow();
        let collapsed = self.collapsed.borrow();
        let mut schemas: Vec<&str> = tables.iter().map(|t| t.schema.as_str()).collect();
        schemas.dedup();
        // Схема одна — заголовок не нужен.
        let headers = schemas.len() > 1;
        let mut rows = Vec::new();
        for schema in schemas {
            let matching: Vec<&TableInfo> = tables
                .iter()
                .filter(|t| t.schema == schema)
                .filter(|t| filter.is_empty() || t.name.to_lowercase().contains(&filter))
                .collect();
            if matching.is_empty() && !filter.is_empty() {
                continue;
            }
            if headers {
                let folded = filter.is_empty() && collapsed.contains(schema);
                self.nav.append(&schema_row(schema, matching.len(), folded));
                rows.push(NavRow::Schema(schema.to_string()));
                if folded {
                    continue;
                }
            }
            for table in matching {
                self.nav.append(&table_row(table, headers));
                rows.push(NavRow::Table(table.clone()));
            }
        }
        *self.nav_rows.borrow_mut() = rows;
        drop((tables, collapsed));
        self.sync_placeholder();
    }

    fn set_nav_note(&self, text: &str) {
        *self.nav_note.borrow_mut() = text.to_string();
        self.sync_placeholder();
    }

    /// Надпись видна, когда в списке нет строк.
    fn sync_placeholder(&self) {
        let empty = self.nav_rows.borrow().is_empty();
        self.nav_placeholder.set_visible(empty);
        if !empty {
            return;
        }
        let searching = !self.search.text().trim().is_empty();
        if searching && !self.tables.borrow().is_empty() {
            self.nav_placeholder.set_text("Ничего не найдено");
        } else {
            self.nav_placeholder.set_text(&self.nav_note.borrow());
        }
    }

    fn show_nav_menu(&self, x: f64, y: f64) {
        let Some(row) = self.nav.row_at_y(y as i32) else {
            return;
        };
        let entry = usize::try_from(row.index())
            .ok()
            .and_then(|index| self.nav_rows.borrow().get(index).cloned());
        let Some(NavRow::Table(table)) = entry else {
            return;
        };
        self.nav.select_row(Some(&row));
        *self.menu_table.borrow_mut() = Some(table);
        let Some(parent) = self.nav_menu.parent() else {
            return;
        };
        let (x, y) = self
            .nav
            .compute_point(&parent, &gtk::graphene::Point::new(x as f32, y as f32))
            .map_or((x as i32, y as i32), |point| {
                (point.x() as i32, point.y() as i32)
            });
        self.nav_menu
            .set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, 1)));
        self.nav_menu.popup();
    }
}

fn schema_row(schema: &str, count: usize, folded: bool) -> gtk::ListBoxRow {
    let arrow = gtk::Image::from_icon_name("pan-down-symbolic");
    arrow.add_css_class("sshdeck-folder-arrow");
    if folded {
        arrow.add_css_class("collapsed");
    }
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.append(&arrow);
    content.append(&gtk::Image::from_icon_name("folder-symbolic"));
    content.append(
        &gtk::Label::builder()
            .label(schema)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build(),
    );
    content.append(
        &gtk::Label::builder()
            .label(count.to_string())
            .css_classes(["dim-label", "caption"])
            .build(),
    );
    gtk::ListBoxRow::builder()
        .child(&content)
        .tooltip_text(format!("Схема {schema}"))
        .build()
}

fn table_row(table: &TableInfo, indented: bool) -> gtk::ListBoxRow {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    if indented {
        content.set_margin_start(18);
    }
    let icon = gtk::Image::from_icon_name(if table.view {
        "view-reveal-symbolic"
    } else {
        "view-grid-symbolic"
    });
    icon.add_css_class("dim-label");
    content.append(&icon);
    content.append(
        &gtk::Label::builder()
            .label(&table.name)
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .build(),
    );
    let kind = if table.view {
        "Представление"
    } else {
        "Таблица"
    };
    gtk::ListBoxRow::builder()
        .child(&content)
        .tooltip_text(format!("{kind} {}.{}", table.schema, table.name))
        .build()
}

/// Таблица результатов с подписью и плашками итогов.
struct Results {
    root: gtk::Box,
    caption: gtk::Label,
    /// Плашки над таблицей: «200 строк», «12 мс».
    rows_pill: gtk::Label,
    time_pill: gtk::Label,
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
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.add_css_class("sshdeck-db-toolbar");
        header.add_css_class("sshdeck-results-header");
        header.append(&caption);
        header.append(&rows_pill);
        header.append(&time_pill);

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

    fn show_hint(&self, text: &str) {
        self.hint.set_text(text);
        self.set_pills(None, None);
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
        self.stack.set_visible_child_name("message");
    }

    fn show_error(&self, heading: &str, message: &str) {
        self.set_pills(None, None);
        self.show_message(true, heading, message);
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
    format!("{n} {word}")
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
