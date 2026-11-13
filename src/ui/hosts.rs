//! Боковая панель: быстрое подключение по адресу, сохранённые подключения
//! (с названиями и вложенными папками), базы данных и хосты из ssh-конфига.
//!
//! Порядок сохранённых подключений задаёт пользователь: строки и папки
//! перетаскиваются мышью (перед строкой, после неё или внутрь папки), а
//! в контекстном меню есть «Выше», «Ниже» и «Переместить в папку…».

mod arrange;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};

use super::databases::{Databases, describe as describe_db, profile_badge};
use super::{Notifier, collapsing_revealer, dialogs, reveal};
use crate::db::DbProfile;
use crate::exchange;
use crate::forward::{self, Forward};
use crate::connections::{
    self, Connection, Connections, FOLDER_COLORS, FolderPath, Loc, Node, Target, join_path, split_path,
};
use arrange::{DropRow, FolderChoice, Mark, display_path};
use crate::probe::{self, Status};
use crate::session;
use crate::settings::Settings;
use crate::ssh_config::{self, HostEntry};

const CANCEL: &str = "cancel";
const ACCEPT: &str = "accept";

/// Стрелка папки; у свёрнутой папки CSS поворачивает её на четверть оборота.
const ICON_FOLDER_ARROW: &str = "pan-down-symbolic";

/// Поля над и под содержимым строки списка.
const ROW_MARGIN: i32 = 2;

/// Как часто перепроверять доступность серверов. Каждая проверка — одно
/// TCP-соединение без входа; редкие проверки не насторожат fail2ban даже
/// в строгом режиме.
const PROBE_INTERVAL_SECS: u32 = 180;

/// Классы значка состояния (`.sshdeck-status.*` в CSS окна).
const STATUS_CLASSES: [&str; 4] = ["checking", "online", "offline", "unknown"];

/// Открывает вкладку: адрес, необязательное название и команда после входа. Возвращает `false`,
/// если адрес не подошёл.
/// Открыть вкладку: адрес и сохранённое подключение, если вкладка
/// открывается из него.
type OpenHandler = Box<dyn Fn(&str, Option<&Connection>) -> bool>;

#[derive(Clone)]
enum RowKind {
    /// Заголовок раздела или подсказка: не выбирается.
    Static,
    /// Папка: место в дереве и полный путь.
    Folder(Loc, FolderPath),
    /// Сохранённое подключение: место в дереве и путь папки, в которой
    /// оно лежит.
    Connection(Loc, FolderPath),
    /// Индекс в списке хостов из ssh-конфига.
    Host(usize),
    /// Индекс в списке баз данных.
    Database(usize),
}

struct Row {
    kind: RowKind,
    widget: gtk::ListBoxRow,
    /// Стрелка «свёрнуто/развёрнуто» у строки папки.
    arrow: Option<gtk::Image>,
    /// Обёртка содержимого строки подключения: через неё строка плавно
    /// сворачивается вместе с папкой.
    revealer: Option<gtk::Revealer>,
    /// Значок «в сети / недоступен» у подключения и хоста.
    status: Option<StatusView>,
    /// Заголовок, на который можно бросить подключение или папку: в начало
    /// или в конец корня.
    root_drop: Option<RootDrop>,
}

#[derive(Clone, Copy)]
enum RootDrop {
    Start,
    End,
}

/// Перетаскиваемый элемент: подключение или папка (её место в дереве).
struct DragItem(Loc);

/// Элемент, о котором помнит открытый диалог. После любых изменений
/// строки перестраиваются, поэтому место проверяется заново.
#[derive(Clone)]
enum Subject {
    Connection(Loc, Connection),
    Folder(FolderPath),
}

impl Subject {
    /// Текущее место элемента.
    fn locate(&self, list: &Connections) -> Option<Loc> {
        match self {
            Self::Connection(loc, connection) => {
                if list.connection(loc) == Some(connection) {
                    Some(loc.clone())
                } else {
                    list.find_connection(connection)
                }
            }
            Self::Folder(path) if path.is_empty() => None,
            Self::Folder(path) => list.folder_loc(path),
        }
    }
}

/// Отображение доступности сервера в строке списка.
struct StatusView {
    /// Адрес в том виде, в каком его получает ssh; по нему хранится
    /// результат проверки.
    address: String,
    dot: gtk::Box,
    content: gtk::Box,
}

impl StatusView {
    fn paint(&self, status: Option<&Status>) {
        let (class, text) = match status {
            None => ("checking", tr!("Checking availability…").to_string()),
            Some(status @ Status::Online { .. }) => ("online", status.describe()),
            Some(status @ Status::Offline(_)) => ("offline", status.describe()),
            Some(status @ Status::Unknown(_)) => ("unknown", status.describe()),
        };
        for other in STATUS_CLASSES {
            if other != class {
                self.dot.remove_css_class(other);
            }
        }
        self.dot.add_css_class(class);
        if class == "offline" {
            self.content.add_css_class("offline");
        } else {
            self.content.remove_css_class("offline");
        }
        self.content
            .set_tooltip_text(Some(&format!("{}\n{text}", self.address)));
    }
}

pub struct HostsSidebar {
    inner: Rc<Inner>,
}

struct Inner {
    me: Weak<Inner>,
    root: adw::ToolbarView,
    entry: gtk::Entry,
    list: gtk::ListBox,
    /// Прокручиваемая область списка; к ней привязано контекстное меню.
    scroll: gtk::ScrolledWindow,
    note: gtk::Label,
    menu: gtk::PopoverMenu,
    notifier: Notifier,
    path: PathBuf,
    connections: RefCell<Connections>,
    hosts: RefCell<Vec<HostEntry>>,
    rows: RefCell<Vec<Row>>,
    /// Пути свёрнутых папок. Хранятся в настройках окна
    /// (`Settings::collapsed_folders`) и пишутся туда при каждом изменении.
    collapsed: RefCell<HashSet<FolderPath>>,
    settings: Rc<RefCell<Settings>>,
    actions: gio::SimpleActionGroup,
    /// Что сейчас перетаскивается (место в дереве).
    dragging: RefCell<Option<Loc>>,
    /// Строка с отметкой места вставки.
    drop_mark: RefCell<Option<gtk::ListBoxRow>>,
    /// Строка, для которой открыто контекстное меню.
    menu_row: Cell<Option<usize>>,
    /// Последний результат проверки по адресу.
    statuses: RefCell<HashMap<String, Status>>,
    /// Адреса, проверка которых ещё идёт.
    checking: RefCell<HashSet<String>>,
    open: RefCell<Option<OpenHandler>>,
    databases: Databases,
    /// Базы данных в том порядке, в каком они показаны.
    db_profiles: RefCell<Vec<DbProfile>>,
}

impl HostsSidebar {
    pub fn new(notifier: Notifier, databases: Databases, settings: Rc<RefCell<Settings>>) -> Self {
        let entry = gtk::Entry::builder()
            .placeholder_text(tr!("Search or user@host"))
            .tooltip_text(tr!(
                "Quick connect: user@host, user@host -p 2222 \
                 or an alias from ~/.ssh/config"
            ))
            .primary_icon_name("system-search-symbolic")
            .secondary_icon_name("go-next-symbolic")
            .secondary_icon_tooltip_text(tr!("Connect"))
            .css_classes(["sshdeck-address"])
            .build();

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .activate_on_single_click(false)
            .css_classes(["navigation-sidebar", "sshdeck-hosts"])
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .vexpand(true)
            .build();
        let note = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .visible(false)
            .css_classes(["caption", "dim-label", "sshdeck-note"])
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&entry);
        content.append(&scroll);
        content.append(&note);

        let add_button = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text(tr!("Save a new connection"))
            .action_name("hosts.new")
            .css_classes(["flat"])
            .build();
        let reload_button = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text(tr!("Reload saved connections and ~/.ssh/config"))
            .action_name("hosts.reload")
            .css_classes(["flat"])
            .build();
        // Своя шапка вместо стандартного заголовка: знак приложения и
        // название, за неё можно таскать окно.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        header.add_css_class("sshdeck-sidebar-header");
        // Кнопки окна, если система ставит их слева.
        header.append(&super::window_controls(gtk::PackType::Start));
        let logo = gtk::Label::builder()
            .label("›_")
            .valign(gtk::Align::Center)
            .css_classes(["sshdeck-logo"])
            .build();
        header.append(&logo);
        header.append(
            &gtk::Label::builder()
                .label("sshdeck")
                .xalign(0.0)
                .hexpand(true)
                .css_classes(["sshdeck-brand"])
                .build(),
        );
        header.append(&add_button);
        header.append(&reload_button);
        let root = adw::ToolbarView::new();
        root.add_css_class("sshdeck-sidebar");
        root.add_top_bar(&gtk::WindowHandle::builder().child(&header).build());
        root.set_content(Some(&content));

        arrange::install_css();
        let menu = gtk::PopoverMenu::from_model(gio::MenuModel::NONE);
        menu.set_parent(&scroll);
        menu.set_has_arrow(false);
        menu.set_halign(gtk::Align::Start);
        {
            let menu = menu.clone();
            scroll.connect_destroy(move |_| menu.unparent());
        }

        // Свёрнутые папки прошлого запуска — до первой перестройки списка
        // в reload().
        let collapsed: HashSet<FolderPath> = settings.borrow().collapsed_folders.iter().cloned().collect();
        let inner = Rc::new_cyclic(|me| Inner {
            me: me.clone(),
            root,
            entry,
            list,
            scroll,
            note,
            menu,
            notifier,
            path: connections::default_path(),
            connections: RefCell::new(Connections::default()),
            hosts: RefCell::new(Vec::new()),
            rows: RefCell::new(Vec::new()),
            collapsed: RefCell::new(collapsed),
            settings,
            actions: gio::SimpleActionGroup::new(),
            dragging: RefCell::new(None),
            drop_mark: RefCell::new(None),
            menu_row: Cell::new(None),
            statuses: RefCell::new(HashMap::new()),
            checking: RefCell::new(HashSet::new()),
            open: RefCell::new(None),
            databases,
            db_profiles: RefCell::new(Vec::new()),
        });
        {
            let weak = Rc::downgrade(&inner);
            inner.databases.connect_changed(move || {
                if let Some(this) = weak.upgrade() {
                    this.rebuild();
                }
            });
        }
        inner.connect_signals();
        inner.reload();
        {
            let weak = Rc::downgrade(&inner);
            glib::timeout_add_seconds_local(PROBE_INTERVAL_SECS, move || match weak.upgrade() {
                Some(this) => {
                    this.probe(false);
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
        }
        Self { inner }
    }

    pub fn widget(&self) -> &adw::ToolbarView {
        &self.inner.root
    }

    /// Задаёт способ открыть вкладку; без него панель ничего не открывает.
    pub fn set_open_handler(
        &self,
        handler: impl Fn(&str, Option<&Connection>) -> bool + 'static,
    ) {
        *self.inner.open.borrow_mut() = Some(Box::new(handler));
    }

    /// Меняет пробросы портов сохранённого подключения (из диалога
    /// пробросов вкладки) и сохраняет файл. Подключение ищется таким, каким
    /// его видела вкладка, а если его с тех пор правили — по названию и
    /// адресу. Возвращает подключение в новом виде.
    pub fn update_saved_forwards(
        &self,
        original: &Connection,
        edit: &dyn Fn(&mut Vec<Forward>),
    ) -> Result<Connection, String> {
        self.inner.update_saved_forwards(original, edit)
    }

    /// Перечитывает сохранённые подключения и ssh-конфиг.
    pub fn reload(&self) {
        self.inner.reload();
    }

    /// Сохраняет все подключения в JSON-файл (выбор файла — диалогом).
    pub fn export_dialog(&self) {
        self.inner.export_dialog();
    }

    /// Добавляет подключения из экспорта sshdeck или ssh_config.
    pub fn import_dialog(&self) {
        self.inner.import_dialog();
    }
}

impl Inner {
    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);

        {
            let weak = weak.clone();
            let open = move || {
                if let Some(this) = weak.upgrade()
                    && this.open(&this.entry.text(), None)
                {
                    this.entry.set_text("");
                }
            };
            let open_on_icon = open.clone();
            self.entry
                .connect_icon_release(move |_, _| open_on_icon());
            self.entry.connect_activate(move |_| open());
        }
        {
            // Набранный текст заодно фильтрует список.
            let weak = weak.clone();
            self.entry.connect_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.apply_collapsed(false);
                }
            });
        }
        {
            let weak = weak.clone();
            self.list.connect_row_activated(move |_, row| {
                let Some(this) = weak.upgrade() else { return };
                if let Some(kind) = this.kind_of(row) {
                    this.activate(&kind);
                }
            });
        }
        {
            let weak = weak.clone();
            let gesture = gtk::GestureClick::builder()
                .button(gdk::BUTTON_SECONDARY)
                .build();
            gesture.connect_pressed(move |gesture, _, x, y| {
                if let Some(this) = weak.upgrade() {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    this.show_menu(x, y);
                }
            });
            self.list.add_controller(gesture);
        }
        self.connect_drop();

        let actions = &self.actions;
        let add = |name: &str, handler: fn(&Rc<Inner>)| {
            let weak = weak.clone();
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    handler(&this);
                }
            });
            actions.add_action(&action);
        };
        add("reload", |this| this.reload());
        add("new", |this| {
            let address = this.entry.text();
            this.edit_dialog("", &address, "", "", &[], None);
        });
        add("connect", |this| {
            if let Some(kind) = this.menu_kind() {
                this.activate(&kind);
            }
        });
        add("edit", |this| {
            if let Some((loc, folder, c)) = this.menu_connection() {
                let folder = join_path(&folder);
                this.edit_dialog(&c.name, &c.address, &folder, &c.startup, &c.forwards, Some((loc, c.clone())));
            }
        });
        add("duplicate", |this| {
            if let Some((_, folder, c)) = this.menu_connection() {
                let name = tr!("{} (copy)", c.name);
                this.edit_dialog(&name, &c.address, &join_path(&folder), &c.startup, &c.forwards, None);
            }
        });
        add("move-up", |this| this.move_by(-1));
        add("move-down", |this| this.move_by(1));
        add("move-to", |this| this.move_dialog());
        add("delete", |this| this.delete_connection());
        add("save-host", |this| {
            let alias = match this.menu_kind() {
                Some(RowKind::Host(index)) => {
                    this.hosts.borrow().get(index).map(|h| h.alias.clone())
                }
                _ => None,
            };
            if let Some(alias) = alias {
                this.edit_dialog(&alias, &alias, "", "", &[], None);
            }
        });
        add("new-in-folder", |this| {
            if let Some((_, folder)) = this.menu_folder() {
                this.edit_dialog("", "", &join_path(&folder), "", &[], None);
            }
        });
        add("db-new", |this| this.databases.create());
        add("db-extensions", |this| this.databases.extensions());
        add("db-edit", |this| {
            if let Some(profile) = this.menu_database() {
                this.databases.edit(&profile);
            }
        });
        add("db-duplicate", |this| {
            if let Some(profile) = this.menu_database() {
                this.databases.duplicate(&profile);
            }
        });
        add("db-delete", |this| {
            if let Some(profile) = this.menu_database() {
                this.databases.delete(&profile);
            }
        });
        add("rename-folder", |this| this.rename_folder());
        add("dissolve-folder", |this| this.dissolve_folder());
        add("delete-folder", |this| this.delete_folder());
        {
            // Параметр — идентификатор цвета; пустая строка убирает цвет.
            let weak = weak.clone();
            let action = gio::SimpleAction::new("folder-color", Some(glib::VariantTy::STRING));
            action.connect_activate(move |_, parameter| {
                let color = parameter.and_then(|value| value.get::<String>());
                if let (Some(this), Some(color)) = (weak.upgrade(), color) {
                    this.set_folder_color(&color);
                }
            });
            actions.add_action(&action);
        }
        self.root.insert_action_group("hosts", Some(actions));
    }

    fn open(&self, target: &str, saved: Option<&Connection>) -> bool {
        match self.open.borrow().as_ref() {
            Some(open) => open(target, saved),
            None => false,
        }
    }

    fn update_saved_forwards(
        &self,
        original: &Connection,
        edit: &dyn Fn(&mut Vec<Forward>),
    ) -> Result<Connection, String> {
        let updated = {
            let mut list = self.connections.borrow_mut();
            let loc = list.find_connection(original).or_else(|| {
                list.walk()
                    .into_iter()
                    .find(|item| {
                        matches!(item.node, Node::Connection(c)
                            if c.name == original.name && c.address == original.address)
                    })
                    .map(|item| item.loc)
            });
            let current = loc.as_ref().and_then(|loc| list.connection(loc).cloned());
            let (Some(loc), Some(current)) = (loc, current) else {
                return Err(tr!(
                    "The saved connection “{}” was renamed or deleted, so the forwards were not saved in it",
                    original.name
                ));
            };
            let mut forwards = current.forwards.clone();
            edit(&mut forwards);
            let updated = current.with_forwards(forwards);
            let folder = list.parent_path(&loc);
            list.replace(&loc, updated.clone(), &folder);
            updated
        };
        self.commit();
        Ok(updated)
    }

    // ----- данные

    fn reload(&self) {
        let home = glib::home_dir();
        let config_path = session::config_override().unwrap_or_else(|| home.join(".ssh/config"));
        let parsed = ssh_config::load(&config_path, &home);
        self.note.set_text(&parsed.warnings.join("\n"));
        self.note.set_visible(!parsed.warnings.is_empty());
        *self.hosts.borrow_mut() = parsed.hosts;

        match Connections::load(&self.path) {
            Ok(list) => {
                *self.connections.borrow_mut() = list;
                // Файл могли поправить руками: забываем пропавшие папки.
                self.forget_missing_folders();
            }
            Err(err) => {
                // Повреждённый файл откладываем в сторону, чтобы следующее
                // сохранение не затёрло то, что в нём ещё можно прочитать.
                let mut aside = self.path.as_os_str().to_owned();
                aside.push(".broken");
                let aside = PathBuf::from(aside);
                let moved = std::fs::rename(&self.path, &aside).is_ok();
                *self.connections.borrow_mut() = Connections::default();
                let tail = if moved {
                    tr!("The file was renamed to {}.", aside.display())
                } else {
                    tr!("Fix the file by hand: the next save will overwrite it.").to_string()
                };
                self.notifier.error(&tr!(
                    "Could not read saved connections: {}\n{}",
                    err,
                    tail
                ));
            }
        }
        self.rebuild();
        self.probe(false);
    }

    // ----- импорт и экспорт

    fn export_dialog(&self) {
        let count = self
            .connections
            .borrow()
            .walk()
            .iter()
            .filter(|item| matches!(item.node, Node::Connection(_)))
            .count();
        if count == 0 {
            self.notifier
                .info(tr!("There are no saved connections to export"));
            return;
        }
        let dialog = gtk::FileDialog::builder()
            .title(tr!("Export Connections"))
            .initial_name(exchange::DEFAULT_FILE_NAME)
            .modal(true)
            .build();
        let weak = self.me.clone();
        dialog.save(
            super::parent_window(&self.root).as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                let (Some(this), Ok(file)) = (weak.upgrade(), result) else {
                    return;
                };
                let Some(path) = file.path() else { return };
                let text = exchange::export(&this.connections.borrow());
                match std::fs::write(&path, text) {
                    Ok(()) => this.notifier.info(&tr!(
                        "Connections exported: {} (passwords are never included)",
                        count
                    )),
                    Err(err) => this
                        .notifier
                        .error(&tr!("Could not write {}: {}", path.display(), err)),
                }
            },
        );
    }

    fn import_dialog(&self) {
        // Без фильтров: ssh_config обычно без расширения.
        let dialog = gtk::FileDialog::builder()
            .title(tr!("Import Connections"))
            .modal(true)
            .build();
        let weak = self.me.clone();
        dialog.open(
            super::parent_window(&self.root).as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                let (Some(this), Ok(file)) = (weak.upgrade(), result) else {
                    return;
                };
                if let Some(path) = file.path() {
                    this.import_file(&path);
                }
            },
        );
    }

    fn import_file(&self, path: &std::path::Path) {
        let current = self.connections.borrow().clone();
        let (list, report) = match exchange::import(&current, path, &glib::home_dir()) {
            Ok(result) => result,
            Err(err) => {
                dialogs::message(&self.root, tr!("Could Not Import Connections"), &err);
                return;
            }
        };
        if report.added > 0 {
            *self.connections.borrow_mut() = list;
            self.commit();
        }
        let mut body = if report.added == 0 {
            tr!("All connections from the file are already in the list.").to_string()
        } else {
            tr!("Connections added: {}", report.added)
        };
        if report.added > 0 && report.skipped > 0 {
            body.push('\n');
            body.push_str(&tr!("Already in the list and skipped: {}", report.skipped));
        }
        if report.from_ssh_config {
            body.push_str("\n\n");
            body.push_str(tr!(
                "The file is an ssh config: only the host address, user and port were taken. \
                 Keys, ProxyJump and other options stay in that file."
            ));
        }
        dialogs::message(&self.root, tr!("Connections Imported"), &body);
    }

    /// Записывает список на диск и перерисовывает панель.
    fn commit(&self) {
        if let Err(err) = self.connections.borrow().save(&self.path) {
            self.notifier
                .error(&tr!("Could not save connections: {}", err));
        }
        self.forget_missing_folders();
        self.rebuild();
        self.probe(true);
    }

    // ----- доступность серверов

    /// Проверяет серверы из списка в фоне. С `only_new` — только те,
    /// о которых ещё ничего не известно (после правки списка).
    fn probe(&self, only_new: bool) {
        let addresses: HashSet<String> = self
            .rows
            .borrow()
            .iter()
            .filter_map(|row| row.status.as_ref().map(|view| view.address.clone()))
            .collect();
        let config = session::config_override();
        for address in addresses {
            if self.checking.borrow().contains(&address)
                || (only_new && self.statuses.borrow().contains_key(&address))
            {
                continue;
            }
            self.checking.borrow_mut().insert(address.clone());
            let weak = self.me.clone();
            let config = config.clone();
            glib::spawn_future_local(async move {
                let target = address.clone();
                // ssh -G и TCP-соединение блокируют: работаем вне потока GTK.
                let status =
                    gio::spawn_blocking(move || probe::check(&target, config.as_deref()))
                        .await
                        .unwrap_or_else(|_| Status::Unknown(tr!("the check was interrupted").to_string()));
                let Some(this) = weak.upgrade() else { return };
                this.checking.borrow_mut().remove(&address);
                this.statuses.borrow_mut().insert(address, status);
                this.show_statuses();
            });
        }
    }

    /// Перекрашивает значки по последним результатам. Пока проверки нет,
    /// значок серый и пульсирует; при повторной проверке остаётся прежний
    /// результат, пока не придёт новый.
    fn show_statuses(&self) {
        let statuses = self.statuses.borrow();
        for view in self.rows.borrow().iter().filter_map(|row| row.status.as_ref()) {
            view.paint(statuses.get(&view.address));
        }
    }

    /// Сохраняет подключение из диалога: правка на месте или новое в конце
    /// папки `folder`.
    fn store(&self, connection: Connection, folder: FolderPath, original: Option<(Loc, Connection)>) {
        self.expand_to(&folder);
        let loc = {
            let mut list = self.connections.borrow_mut();
            let position = original.and_then(|(loc, original)| {
                Subject::Connection(loc, original).locate(&list)
            });
            match position {
                Some(index) => list.replace(&index, connection, &folder),
                None => Some(list.add(connection, &folder)),
            }
        };
        self.commit();
        if let Some(loc) = loc {
            self.select_loc(&loc);
        }
    }

    /// Убирает из свёрнутых исчезнувшие папки (удалённые, убранные,
    /// опустевшие) и сохраняет свёрнутые папки, если они изменились.
    fn forget_missing_folders(&self) {
        let folders = self.connections.borrow().folders();
        arrange::keep_existing(&mut self.collapsed.borrow_mut(), &folders);
        self.save_collapsed();
    }

    /// Записывает свёрнутые папки в настройки, если они изменились.
    fn save_collapsed(&self) {
        let current: BTreeSet<FolderPath> = self.collapsed.borrow().iter().cloned().collect();
        if self.settings.borrow().collapsed_folders == current {
            return;
        }
        self.settings.borrow_mut().collapsed_folders = current;
        if let Err(err) = self.settings.borrow().save() {
            self.notifier
                .error(&tr!("Could not save settings: {}", err));
        }
    }

    /// Раскрывает папку `path` и всех её родителей.
    fn expand_to(&self, path: &[String]) {
        let mut collapsed = self.collapsed.borrow_mut();
        for depth in 1..=path.len() {
            collapsed.remove(&path[..depth]);
        }
    }

    /// Выделяет строку подключения или папки после перестройки списка.
    fn select_loc(&self, loc: &[usize]) {
        let rows = self.rows.borrow();
        let row = rows.iter().find(|row| match &row.kind {
            RowKind::Connection(at, _) | RowKind::Folder(at, _) => at.as_slice() == loc,
            _ => false,
        });
        if let Some(row) = row {
            self.list.select_row(Some(&row.widget));
        }
    }

    // ----- список

    fn rebuild(&self) {
        self.rows.borrow_mut().clear();
        self.list.remove_all();

        let mut rows = Vec::new();
        {
            let connections = self.connections.borrow();
            let mut saved = static_row(heading_label(tr!("Saved")));
            saved.root_drop = Some(RootDrop::Start);
            rows.push(saved);
            if connections.is_empty() {
                rows.push(static_row(hint_label(tr!(
                    "Nothing here yet. Press “+” at the top to save a connection \
                     with a name and folder."
                ))));
            }
            for item in connections.walk() {
                let depth = item.depth();
                match item.node {
                    Node::Folder(folder) => {
                        let mut path = item.parent.clone();
                        path.push(folder.name.clone());
                        rows.push(self.folder_row(
                            item.loc.clone(),
                            path,
                            folder.count(),
                            folder.color.as_deref(),
                        ));
                    }
                    Node::Connection(c) => {
                        let (widget, revealer, status) =
                            entry_row(&c.name, &c.address, &c.address, depth);
                        self.attach_drag(&widget, &item.loc);
                        let mut row = Row::new(RowKind::Connection(item.loc.clone(), item.parent.clone()), widget);
                        row.revealer = Some(revealer);
                        row.status = Some(status);
                        rows.push(row);
                    }
                }
            }

            let profiles = self.databases.profiles();
            rows.push(self.databases_heading());
            if profiles.is_empty() {
                rows.push(static_row(hint_label(tr!(
                    "PostgreSQL, MySQL, MariaDB or SQLite — press “+” next to the \
                     heading. Servers can be reached over SSH."
                ))));
            }
            for (index, profile) in profiles.iter().enumerate() {
                let (widget, revealer) = database_row(profile);
                let mut row = Row::new(RowKind::Database(index), widget);
                row.revealer = Some(revealer);
                rows.push(row);
            }
            *self.db_profiles.borrow_mut() = profiles;

            let hosts = self.hosts.borrow();
            if !hosts.is_empty() {
                rows.push(static_row(heading_label(tr!("From ~/.ssh/config"))));
            }
            for (index, host) in hosts.iter().enumerate() {
                let (widget, revealer, status) =
                    entry_row(&host.alias, &host.detail(), &host.alias, 0);
                let mut row = Row::new(RowKind::Host(index), widget);
                row.revealer = Some(revealer);
                row.status = Some(status);
                rows.push(row);
            }
        }
        for row in &rows {
            self.list.append(&row.widget);
        }
        *self.rows.borrow_mut() = rows;
        self.apply_collapsed(false);
        self.show_statuses();
    }

    /// Заголовок раздела баз данных с кнопкой «добавить».
    fn databases_heading(&self) -> Row {
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text(tr!("New database connection"))
            .valign(gtk::Align::Center)
            .css_classes(["flat", "sshdeck-section-button"])
            .build();
        let databases = self.databases.clone();
        add.connect_clicked(move |_| databases.create());
        let label = heading_label(tr!("Databases"));
        label.set_hexpand(true);
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        content.append(&label);
        content.append(&add);
        let widget = gtk::ListBoxRow::builder()
            .child(&content)
            .selectable(false)
            .activatable(false)
            .build();
        let mut row = Row::new(RowKind::Static, widget);
        // Сюда бросают то, что должно оказаться в конце сохранённых.
        row.root_drop = Some(RootDrop::End);
        row
    }

    fn folder_row(&self, loc: Loc, path: FolderPath, count: usize, color: Option<&str>) -> Row {
        let depth = path.len().saturating_sub(1);
        let folder = path.last().cloned().unwrap_or_default();
        let arrow = gtk::Image::from_icon_name(ICON_FOLDER_ARROW);
        arrow.add_css_class("sshdeck-folder-arrow");
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(ROW_MARGIN);
        content.set_margin_bottom(ROW_MARGIN);
        content.set_margin_start(depth as i32 * arrange::INDENT);
        content.append(&arrow);
        let icon = gtk::Image::from_icon_name("folder-symbolic");
        let name_label = gtk::Label::builder()
            .label(&folder)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        if let Some(color) = color {
            let class = format!("sshdeck-folder-{color}");
            icon.add_css_class(&class);
            name_label.add_css_class(&class);
        }
        content.append(&icon);
        content.append(&name_label);
        content.append(
            &gtk::Label::builder()
                .label(count.to_string())
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        // Вложенная папка прячется вместе с родительской, поэтому строка
        // папки тоже сворачивается плавно.
        let revealer = collapsing_revealer(gtk::RevealerTransitionType::SlideDown, &content);
        revealer.set_reveal_child(true);
        let tooltip = if depth > 0 {
            format!("{}\n{}", display_path(&path), tr!("Click to collapse or expand the folder"))
        } else {
            tr!("Click to collapse or expand the folder").to_string()
        };
        let widget = gtk::ListBoxRow::builder()
            .child(&revealer)
            .tooltip_text(tooltip)
            .build();

        let gesture = gtk::GestureClick::builder()
            .button(gdk::BUTTON_PRIMARY)
            .build();
        let weak = self.me.clone();
        let toggled = path.clone();
        gesture.connect_released(move |_, n_press, _, _| {
            // Второй щелчок двойного нажатия приходит как активация строки.
            if n_press == 1
                && let Some(this) = weak.upgrade()
            {
                this.toggle_folder(&toggled);
            }
        });
        widget.add_controller(gesture);
        self.attach_drag(&widget, &loc);

        let mut row = Row::new(RowKind::Folder(loc, path), widget);
        row.arrow = Some(arrow);
        row.revealer = Some(revealer);
        row
    }

    fn toggle_folder(&self, folder: &[String]) {
        {
            let mut collapsed = self.collapsed.borrow_mut();
            if !collapsed.remove(folder) {
                collapsed.insert(folder.to_vec());
            }
        }
        self.save_collapsed();
        self.apply_collapsed(true);
    }

    /// Показывает и прячет строки: свёрнутые папки (со всем поддеревом) и
    /// поиск по тексту, набранному в поле адреса. С `animate` строки
    /// сворачиваются плавно (щелчок по папке); фильтр при наборе текста
    /// срабатывает сразу.
    fn apply_collapsed(&self, animate: bool) {
        let needle = self.entry.text().trim().to_lowercase();
        let matches = |title: &str, detail: &str| {
            needle.is_empty()
                || title.to_lowercase().contains(&needle)
                || detail.to_lowercase().contains(&needle)
        };
        // Совпадение с именем любой папки на пути показывает всё её
        // содержимое.
        let path_matches = |path: &[String]| path.iter().any(|name| matches(name, ""));
        let collapsed = self.collapsed.borrow();
        // Строка спрятана, если свёрнута одна из папок, в которых она лежит.
        let folded = |parent: &[String]| (1..=parent.len()).any(|depth| collapsed.contains(&parent[..depth]));
        let connections = self.connections.borrow();
        let hosts = self.hosts.borrow();
        let profiles = self.db_profiles.borrow();
        let rows = self.rows.borrow();
        // При поиске: папки, в которых лежат найденные подключения.
        let found: Vec<&FolderPath> = if needle.is_empty() {
            Vec::new()
        } else {
            rows.iter()
                .filter_map(|row| match &row.kind {
                    RowKind::Connection(loc, parent) => connections
                        .connection(loc)
                        .filter(|c| matches(&c.name, &c.address) || path_matches(parent))
                        .map(|_| parent),
                    _ => None,
                })
                .collect()
        };
        for row in rows.iter() {
            match &row.kind {
                RowKind::Folder(_, path) => {
                    if let Some(arrow) = &row.arrow {
                        if collapsed.contains(path) {
                            arrow.add_css_class("collapsed");
                        } else {
                            arrow.remove_css_class("collapsed");
                        }
                    }
                    let visible = if needle.is_empty() {
                        !folded(&path[..path.len().saturating_sub(1)])
                    } else {
                        // Папка видна, если совпало её имя (или имя
                        // родителя) или в ней есть находки: так у найденного
                        // подключения виден весь путь папок.
                        path_matches(path) || found.iter().any(|parent| parent.starts_with(path))
                    };
                    row.set_shown(visible, animate);
                }
                RowKind::Connection(loc, parent) => {
                    let visible = connections.connection(loc).is_some_and(|c| {
                        if needle.is_empty() {
                            !folded(parent)
                        } else {
                            matches(&c.name, &c.address) || path_matches(parent)
                        }
                    });
                    row.set_shown(visible, animate);
                }
                RowKind::Host(index) => {
                    let visible = hosts
                        .get(*index)
                        .is_some_and(|h| matches(&h.alias, &h.detail()));
                    row.set_shown(visible, animate);
                }
                RowKind::Database(index) => {
                    let visible = profiles
                        .get(*index)
                        .is_some_and(|p| matches(&p.name, &describe_db(p)));
                    row.set_shown(visible, animate);
                }
                RowKind::Static => {}
            }
        }
    }

    fn kind_of(&self, widget: &gtk::ListBoxRow) -> Option<RowKind> {
        self.rows
            .borrow()
            .iter()
            .find(|row| &row.widget == widget)
            .map(|row| row.kind.clone())
    }

    /// Двойной щелчок или Enter: подключение открывается, папка
    /// сворачивается.
    fn activate(&self, kind: &RowKind) {
        match kind {
            RowKind::Connection(loc, _) => {
                let connection = self.connections.borrow().connection(loc).cloned();
                if let Some(c) = connection {
                    self.open(&c.address, Some(&c));
                }
            }
            RowKind::Host(index) => {
                let alias = self.hosts.borrow().get(*index).map(|h| h.alias.clone());
                if let Some(alias) = alias {
                    self.open(&alias, None);
                }
            }
            RowKind::Database(index) => {
                let profile = self.db_profiles.borrow().get(*index).cloned();
                if let Some(profile) = profile {
                    self.databases.connect(&profile);
                }
            }
            RowKind::Folder(_, folder) => self.toggle_folder(folder),
            RowKind::Static => {}
        }
    }

    // ----- перетаскивание

    /// Строку подключения или папки можно перетащить в другое место списка.
    fn attach_drag(&self, widget: &gtk::ListBoxRow, loc: &[usize]) {
        let source = gtk::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .build();
        let weak = self.me.clone();
        let loc = loc.to_vec();
        source.connect_prepare(move |source, x, y| {
            let this = weak.upgrade()?;
            *this.dragging.borrow_mut() = Some(loc.clone());
            if let Some(widget) = source.widget() {
                let icon = gtk::WidgetPaintable::new(Some(&widget));
                source.set_icon(Some(&icon), x as i32, y as i32);
            }
            let value = glib::BoxedAnyObject::new(DragItem(loc.clone()));
            Some(gdk::ContentProvider::for_value(&value.to_value()))
        });
        let weak = self.me.clone();
        source.connect_drag_end(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                this.dragging.borrow_mut().take();
                this.clear_drop_mark();
            }
        });
        widget.add_controller(source);
    }

    /// Список принимает только свои строки: значение — объект процесса,
    /// из другого приложения его не перетащить.
    fn connect_drop(self: &Rc<Self>) {
        let drop = gtk::DropTarget::new(glib::BoxedAnyObject::static_type(), gdk::DragAction::MOVE);
        let weak = Rc::downgrade(self);
        drop.connect_motion(move |_, _, y| {
            let Some(this) = weak.upgrade() else {
                return gdk::DragAction::empty();
            };
            let source = this.dragging.borrow().clone();
            let spot = source.and_then(|source| this.drop_spot(&source, y));
            this.clear_drop_mark();
            let Some((row, _, mark)) = spot else {
                return gdk::DragAction::empty();
            };
            match mark {
                Mark::Above => row.add_css_class(arrange::DROP_BEFORE_CLASS),
                Mark::Below => row.add_css_class(arrange::DROP_AFTER_CLASS),
                Mark::Inside => this.list.drag_highlight_row(&row),
            }
            *this.drop_mark.borrow_mut() = Some(row);
            gdk::DragAction::MOVE
        });
        let weak = Rc::downgrade(self);
        drop.connect_leave(move |_| {
            if let Some(this) = weak.upgrade() {
                this.clear_drop_mark();
            }
        });
        let weak = Rc::downgrade(self);
        drop.connect_drop(move |_, value, _, y| {
            let Some(this) = weak.upgrade() else { return false };
            this.clear_drop_mark();
            let Some(source) = value
                .get::<glib::BoxedAnyObject>()
                .ok()
                .map(|object| object.borrow::<DragItem>().0.clone())
            else {
                return false;
            };
            let Some((_, target, _)) = this.drop_spot(&source, y) else {
                return false;
            };
            // Список перестраивается уже после того, как GTK закончит
            // перетаскивание: строка-источник ещё нужна ему до drag-end.
            let weak = Rc::downgrade(&this);
            glib::idle_add_local_once(move || {
                if let Some(this) = weak.upgrade() {
                    this.move_node(&source, |list| list.move_to(&source, &target));
                }
            });
            true
        });
        self.list.add_controller(drop);
    }

    /// Строка под указателем и что будет, если отпустить элемент `source`.
    fn drop_spot(&self, source: &[usize], y: f64) -> Option<(gtk::ListBoxRow, Target, Mark)> {
        let widget = self.list.row_at_y(y as i32)?;
        let bounds = widget.compute_bounds(&self.list)?;
        let fraction = if bounds.height() > 0.0 {
            (y - f64::from(bounds.y())) / f64::from(bounds.height())
        } else {
            0.5
        };
        let rows = self.rows.borrow();
        let row = rows.iter().find(|row| row.widget == widget)?;
        let collapsed = self.collapsed.borrow();
        let spot = match (&row.kind, row.root_drop) {
            (RowKind::Connection(loc, _), _) => DropRow::Connection(loc),
            (RowKind::Folder(loc, path), _) => DropRow::Folder {
                loc,
                collapsed: collapsed.contains(path),
            },
            (_, Some(RootDrop::Start)) => DropRow::Start,
            (_, Some(RootDrop::End)) => DropRow::End,
            _ => return None,
        };
        let root_len = self.connections.borrow().walk().iter().filter(|item| item.depth() == 0).count();
        let (target, mark) = arrange::drop_target(source, spot, fraction, root_len)?;
        Some((widget, target, mark))
    }

    fn clear_drop_mark(&self) {
        if let Some(row) = self.drop_mark.borrow_mut().take() {
            row.remove_css_class(arrange::DROP_BEFORE_CLASS);
            row.remove_css_class(arrange::DROP_AFTER_CLASS);
        }
        self.list.drag_unhighlight_row();
    }

    /// Переносит элемент `loc` (`apply` меняет список и возвращает новое
    /// место): свёрнутые папки из поддерева остаются свёрнутыми, папка, куда
    /// попал элемент, раскрывается, а сам он выделяется.
    fn move_node(&self, loc: &[usize], apply: impl FnOnce(&mut Connections) -> Option<Loc>) {
        let new = {
            let mut list = self.connections.borrow_mut();
            let old_path = list.folder_path(loc);
            let Some(new) = apply(&mut list) else { return };
            if let (Some(old), Some(path)) = (old_path, list.folder_path(&new)) {
                arrange::rebase_paths(&mut self.collapsed.borrow_mut(), &old, &path);
            }
            self.expand_to(&list.parent_path(&new));
            new
        };
        self.commit();
        self.select_loc(&new);
    }

    // ----- контекстное меню

    fn show_menu(&self, x: f64, y: f64) {
        let row = self.list.row_at_y(y as i32);
        let index = row.as_ref().and_then(|row| {
            self.rows
                .borrow()
                .iter()
                .position(|candidate| &candidate.widget == row)
        });
        self.menu_row.set(index);
        let kind = self.menu_kind().unwrap_or(RowKind::Static);
        if let Some(row) = row.filter(|_| !matches!(kind, RowKind::Static)) {
            self.list.select_row(Some(&row));
        }

        let menu = gio::Menu::new();
        let section = |items: &[(&str, &str)]| {
            let section = gio::Menu::new();
            for (label, action) in items {
                section.append(Some(label), Some(action));
            }
            menu.append_section(None, &section);
        };
        let arrange_section = || {
            section(&[
                (tr!("Move Up"), "hosts.move-up"),
                (tr!("Move Down"), "hosts.move-down"),
                (tr!("Move to Folder…"), "hosts.move-to"),
            ]);
        };
        if let RowKind::Connection(loc, _) | RowKind::Folder(loc, _) = &kind {
            let list = self.connections.borrow();
            self.set_action_enabled("move-up", list.can_move_by(loc, -1));
            self.set_action_enabled("move-down", list.can_move_by(loc, 1));
        }
        match kind {
            RowKind::Connection(..) => {
                section(&[
                    (tr!("Connect"), "hosts.connect"),
                    (tr!("Edit…"), "hosts.edit"),
                    (tr!("Duplicate…"), "hosts.duplicate"),
                ]);
                arrange_section();
                section(&[(tr!("Delete…"), "hosts.delete")]);
            }
            RowKind::Folder(..) => {
                section(&[
                    (tr!("New Connection in Folder…"), "hosts.new-in-folder"),
                    (tr!("Rename Folder…"), "hosts.rename-folder"),
                ]);
                arrange_section();
                let colors = gio::Menu::new();
                for (id, label) in FOLDER_COLORS {
                    colors.append(
                        Some(crate::i18n::tr(label)),
                        Some(&format!("hosts.folder-color::{id}")),
                    );
                }
                colors.append(Some(tr!("No Color")), Some("hosts.folder-color::"));
                menu.append_section(Some(tr!("Folder Color")), &colors);
                section(&[
                    (tr!("Remove Folder, Keep Connections"), "hosts.dissolve-folder"),
                    (tr!("Delete Folder and Connections…"), "hosts.delete-folder"),
                ]);
            }
            RowKind::Host(_) => section(&[
                (tr!("Connect"), "hosts.connect"),
                (tr!("Save to Connections…"), "hosts.save-host"),
            ]),
            RowKind::Database(_) => {
                section(&[
                    (tr!("Connect"), "hosts.connect"),
                    (tr!("Edit…"), "hosts.db-edit"),
                    (tr!("Duplicate…"), "hosts.db-duplicate"),
                ]);
                section(&[(tr!("Delete…"), "hosts.db-delete")]);
            }
            RowKind::Static => section(&[
                (tr!("New Connection…"), "hosts.new"),
                (tr!("New Database Connection…"), "hosts.db-new"),
                (tr!("Database Extensions…"), "hosts.db-extensions"),
            ]),
        }
        self.menu.set_menu_model(Some(&menu));
        // Меню принадлежит области прокрутки, а щелчок пришёл в координатах
        // списка.
        let (x, y) = self
            .list
            .compute_point(&self.scroll, &gtk::graphene::Point::new(x as f32, y as f32))
            .map_or((x as i32, y as i32), |point| {
                (point.x() as i32, point.y() as i32)
            });
        self.menu
            .set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, 1)));
        self.menu.popup();
    }

    fn set_action_enabled(&self, name: &str, enabled: bool) {
        if let Some(action) = self
            .actions
            .lookup_action(name)
            .and_downcast::<gio::SimpleAction>()
        {
            action.set_enabled(enabled);
        }
    }

    fn menu_kind(&self) -> Option<RowKind> {
        let index = self.menu_row.get()?;
        self.rows.borrow().get(index).map(|row| row.kind.clone())
    }

    /// Подключение под меню: место, путь его папки и само подключение.
    fn menu_connection(&self) -> Option<(Loc, FolderPath, Connection)> {
        match self.menu_kind()? {
            RowKind::Connection(loc, folder) => {
                let connection = self.connections.borrow().connection(&loc).cloned()?;
                Some((loc, folder, connection))
            }
            _ => None,
        }
    }

    fn menu_database(&self) -> Option<DbProfile> {
        match self.menu_kind()? {
            RowKind::Database(index) => self.db_profiles.borrow().get(index).cloned(),
            _ => None,
        }
    }

    /// Папка под меню: место и полный путь.
    fn menu_folder(&self) -> Option<(Loc, FolderPath)> {
        match self.menu_kind()? {
            RowKind::Folder(loc, path) => Some((loc, path)),
            _ => None,
        }
    }

    /// Подключение или папка под меню.
    fn menu_subject(&self) -> Option<Subject> {
        if let Some((loc, _, connection)) = self.menu_connection() {
            return Some(Subject::Connection(loc, connection));
        }
        self.menu_folder().map(|(_, path)| Subject::Folder(path))
    }

    // ----- действия

    fn delete_connection(self: &Rc<Self>) {
        let Some((loc, _, connection)) = self.menu_connection() else {
            return;
        };
        let weak = Rc::downgrade(self);
        dialogs::confirm(
            &self.root,
            tr!("Delete Connection?"),
            &tr!(
                "“{}” ({}) will be removed from the list. Nothing changes on the server.",
                connection.name,
                connection.address
            ),
            tr!("Delete"),
            true,
            move || {
                let Some(this) = weak.upgrade() else { return };
                {
                    let mut list = this.connections.borrow_mut();
                    if let Some(loc) = Subject::Connection(loc, connection).locate(&list) {
                        list.remove(&loc);
                    }
                }
                this.commit();
            },
        );
    }

    /// «Выше» / «Ниже»: элемент меняется местами с соседом в своей папке.
    fn move_by(&self, delta: isize) {
        let Some(loc) = self.menu_subject().and_then(|s| s.locate(&self.connections.borrow())) else {
            return;
        };
        self.move_node(&loc, |list| list.move_by(&loc, delta));
    }

    /// «Переместить в папку…»: выбор из существующих папок или новый путь.
    fn move_dialog(self: &Rc<Self>) {
        let Some(subject) = self.menu_subject() else {
            return;
        };
        let (current, own_path, choices) = {
            let list = self.connections.borrow();
            let Some(loc) = subject.locate(&list) else { return };
            let own_path = list.folder_path(&loc);
            // Папку нельзя перенести в неё саму, поэтому её поддерево в
            // списке не показывается.
            let choices: Vec<FolderChoice> = list
                .folders()
                .into_iter()
                .filter(|path| !own_path.as_ref().is_some_and(|own| path.starts_with(own)))
                .map(|path| {
                    let color = list
                        .folder_loc(&path)
                        .and_then(|loc| list.folder_color(&loc).map(str::to_string));
                    FolderChoice { path, color }
                })
                .collect();
            (join_path(&list.parent_path(&loc)), own_path, choices)
        };

        let path_row = adw::EntryRow::builder()
            .title(tr!("Folder path"))
            .text(&current)
            .activates_default(true)
            .tooltip_text(folder_path_hint())
            .build();
        let fields = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        fields.append(&path_row);
        let weak_row = path_row.downgrade();
        let picker = arrange::folder_picker(&choices, Some(tr!("Top Level (No Folder)")), move |path| {
            if let Some(row) = weak_row.upgrade() {
                row.set_text(&path);
                // Фокус — обратно в поле, чтобы путь можно было дописать.
                row.grab_focus();
                row.set_position(-1);
            }
        });
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(260)
            .child(&picker)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_width_request(380);
        content.append(&fields);
        content.append(&scroll);

        let name = match &subject {
            Subject::Connection(_, c) => c.name.clone(),
            Subject::Folder(path) => display_path(path),
        };
        let dialog = adw::AlertDialog::new(
            Some(tr!("Move to Folder")),
            Some(&tr!(
                "Where to move “{}”? Pick a folder or type a path: subfolders are separated by \
                 “/”, missing folders are created. An empty path means the top level.",
                name
            )),
        );
        dialog.add_responses(&[(CANCEL, tr!("Cancel")), (ACCEPT, tr!("Move"))]);
        dialog.set_close_response(CANCEL);
        dialog.set_default_response(Some(ACCEPT));
        dialog.set_response_appearance(ACCEPT, adw::ResponseAppearance::Suggested);
        dialog.set_extra_child(Some(&content));
        dialog.set_focus(Some(&path_row));

        // Папку нельзя перенести в неё саму или во вложенную в неё.
        let check = {
            let own_path = own_path.clone();
            move |dialog: &adw::AlertDialog, row: &adw::EntryRow| {
                let inside = own_path
                    .as_ref()
                    .is_some_and(|own| split_path(&row.text()).starts_with(own));
                dialog.set_response_enabled(ACCEPT, !inside);
                if inside {
                    row.add_css_class("error");
                    row.set_tooltip_text(Some(tr!("A folder cannot be moved into itself.")));
                } else {
                    row.remove_css_class("error");
                    row.set_tooltip_text(Some(&folder_path_hint()));
                }
            }
        };
        check(&dialog, &path_row);
        let weak_dialog = dialog.downgrade();
        path_row.connect_changed(move |row| {
            if let Some(dialog) = weak_dialog.upgrade() {
                check(&dialog, row);
            }
        });

        let weak = Rc::downgrade(self);
        dialog.choose(Some(&self.root), gio::Cancellable::NONE, move |response| {
            if response != ACCEPT {
                return;
            }
            let Some(this) = weak.upgrade() else { return };
            let Some(loc) = subject.locate(&this.connections.borrow()) else { return };
            let folder = split_path(&path_row.text());
            this.move_node(&loc, |list| list.move_into(&loc, &folder));
        });
    }

    fn rename_folder(self: &Rc<Self>) {
        let Some((_, path)) = self.menu_folder() else {
            return;
        };
        let current = path.last().cloned().unwrap_or_default();
        let weak = Rc::downgrade(self);
        dialogs::prompt_text(
            &self.root,
            tr!("Rename Folder"),
            tr!("If a folder with this name already exists, the connections will be merged into it."),
            &current.clone(),
            tr!("Rename"),
            move |name| {
                let Some(this) = weak.upgrade() else { return };
                let name = connections::clean(&name);
                if name.is_empty() || name == current {
                    return;
                }
                let Some(loc) = Subject::Folder(path.clone()).locate(&this.connections.borrow()) else {
                    return;
                };
                // Вложенные папки переезжают вместе с переименованной; их
                // свёрнутость сохраняется в move_node.
                this.move_node(&loc, |list| list.rename_folder(&loc, &name));
            },
        );
    }

    fn set_folder_color(&self, color: &str) {
        let Some((loc, _)) = self.menu_folder() else {
            return;
        };
        self.connections
            .borrow_mut()
            .set_folder_color(&loc, Some(color).filter(|color| !color.is_empty()));
        self.commit();
    }

    /// Убирает папку: подключения и вложенные папки поднимаются на уровень
    /// выше.
    fn dissolve_folder(&self) {
        let Some((loc, path)) = self.menu_folder() else {
            return;
        };
        if !self.connections.borrow_mut().dissolve_folder(&loc) {
            return;
        }
        arrange::lift_paths(&mut self.collapsed.borrow_mut(), &path);
        self.commit();
        self.notifier
            .info(&tr!("Folder “{}” removed, connections kept", display_path(&path)));
    }

    fn delete_folder(self: &Rc<Self>) {
        let Some((loc, path)) = self.menu_folder() else {
            return;
        };
        let (count, nested) = {
            let list = self.connections.borrow();
            let Some(folder) = list.folder(&loc) else { return };
            let nested = folder.children.iter().any(|node| matches!(node, Node::Folder(_)));
            (folder.count(), nested)
        };
        let name = display_path(&path);
        let body = if nested {
            trn!(
                "Folder “{1}”, its subfolders and {0} saved connection in them will be removed \
                 from the list. Nothing changes on the servers.",
                "Folder “{1}”, its subfolders and {0} saved connections in them will be removed \
                 from the list. Nothing changes on the servers.",
                count,
                count,
                name
            )
        } else {
            trn!(
                "Folder “{1}” and {0} saved connection in it will be removed from the list. \
                 Nothing changes on the servers.",
                "Folder “{1}” and {0} saved connections in it will be removed from the list. \
                 Nothing changes on the servers.",
                count,
                count,
                name
            )
        };
        let weak = Rc::downgrade(self);
        dialogs::confirm(
            &self.root,
            tr!("Delete Folder and Connections?"),
            &body,
            tr!("Delete"),
            true,
            move || {
                let Some(this) = weak.upgrade() else { return };
                {
                    let mut list = this.connections.borrow_mut();
                    if let Some(loc) = Subject::Folder(path.clone()).locate(&list) {
                        list.delete_folder(&loc);
                    }
                }
                arrange::forget_paths(&mut this.collapsed.borrow_mut(), &path);
                this.commit();
            },
        );
    }

    /// Диалог создания (`original == None`) или изменения подключения.
    /// `folder` — путь папки в том виде, в каком его вводят в поле.
    fn edit_dialog(
        self: &Rc<Self>,
        name: &str,
        address: &str,
        folder: &str,
        startup: &str,
        forwards: &[Forward],
        original: Option<(Loc, Connection)>,
    ) {
        let name_row = adw::EntryRow::builder()
            .title(trc!("connection or command", "Name"))
            .text(name)
            .activates_default(true)
            .build();
        let address_row = adw::EntryRow::builder()
            .title(tr!("Address: user@host or user@host -p 2222"))
            .text(address)
            .activates_default(true)
            .build();
        let folder_row = adw::EntryRow::builder()
            .title(tr!("Folder (optional)"))
            .text(folder)
            .activates_default(true)
            .tooltip_text(folder_path_hint())
            .build();

        let choices: Vec<FolderChoice> = {
            let list = self.connections.borrow();
            list.walk()
                .into_iter()
                .filter_map(|item| match item.node {
                    Node::Folder(f) => {
                        let mut path = item.parent;
                        path.push(f.name.clone());
                        Some(FolderChoice {
                            path,
                            color: f.color.clone(),
                        })
                    }
                    Node::Connection(_) => None,
                })
                .collect()
        };
        if !choices.is_empty() {
            let popover = gtk::Popover::new();
            let row = folder_row.downgrade();
            let weak_popover = popover.downgrade();
            let picker = arrange::folder_picker(&choices, None, move |path| {
                if let Some(popover) = weak_popover.upgrade() {
                    popover.popdown();
                }
                if let Some(row) = row.upgrade() {
                    row.set_text(&path);
                    // Фокус — обратно в поле, чтобы путь можно было дописать.
                    row.grab_focus();
                    row.set_position(-1);
                }
            });
            popover.set_child(Some(
                &gtk::ScrolledWindow::builder()
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .propagate_natural_height(true)
                    .propagate_natural_width(true)
                    .max_content_height(320)
                    .child(&picker)
                    .build(),
            ));
            folder_row.add_suffix(
                &gtk::MenuButton::builder()
                    .icon_name("folder-symbolic")
                    .tooltip_text(tr!("Choose an existing folder"))
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .popover(&popover)
                    .build(),
            );
        }

        let fields = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .width_request(380)
            .build();
        fields.append(&name_row);
        fields.append(&address_row);
        fields.append(&folder_row);
        let startup_row = adw::EntryRow::builder()
            .title(tr!("Command after login (optional)"))
            .text(startup)
            .activates_default(true)
            .build();
        fields.append(&startup_row);
        let forwards_row = adw::EntryRow::builder()
            .title(tr!("Port forwards (optional)"))
            .text(forward::format_list(forwards))
            .activates_default(true)
            .tooltip_text(forwards_hint())
            .build();
        fields.append(&forwards_row);

        let heading = if original.is_some() {
            tr!("Edit Connection")
        } else {
            tr!("New Connection")
        };
        let dialog = adw::AlertDialog::new(
            Some(heading),
            Some(tr!("Passwords are not saved: ssh still asks for them in the terminal.")),
        );
        dialog.add_responses(&[(CANCEL, tr!("Cancel")), (ACCEPT, tr!("Save"))]);
        dialog.set_close_response(CANCEL);
        dialog.set_default_response(Some(ACCEPT));
        dialog.set_response_appearance(ACCEPT, adw::ResponseAppearance::Suggested);
        dialog.set_extra_child(Some(&fields));
        dialog.set_focus(Some(if address.trim().is_empty() && !name.trim().is_empty() {
            &address_row
        } else {
            &name_row
        }));

        sync_validity(&dialog, &address_row, &forwards_row);
        for row in [&address_row, &forwards_row] {
            let (weak_dialog, address, forwards) =
                (dialog.downgrade(), address_row.downgrade(), forwards_row.downgrade());
            row.connect_changed(move |_| {
                if let (Some(dialog), Some(address), Some(forwards)) =
                    (weak_dialog.upgrade(), address.upgrade(), forwards.upgrade())
                {
                    sync_validity(&dialog, &address, &forwards);
                }
            });
        }

        let weak = Rc::downgrade(self);
        dialog.choose(Some(&self.root), gio::Cancellable::NONE, move |response| {
            if response != ACCEPT {
                return;
            }
            let Some(this) = weak.upgrade() else { return };
            let Ok(forwards) = forward::parse_list(&forwards_row.text()) else { return };
            let connection = Connection::new(&name_row.text(), &address_row.text())
                .with_startup(&startup_row.text())
                .with_forwards(forwards);
            if session::validate_target(&connection.address).is_ok() {
                this.store(connection, split_path(&folder_row.text()), original);
            }
        });
    }
}

/// Подсказка к полю пути папки.
fn folder_path_hint() -> String {
    tr!(
        "Subfolders are separated by “/”, for example Work/Servers. \
         To use “/” inside a folder name, write “\\/”."
    )
    .to_string()
}

/// Подсказка к полю пробросов портов.
fn forwards_hint() -> String {
    tr!(
        "Set up automatically after login, as in ssh: -L 8080:localhost:80 (a port here \
         leads to a host seen from the server), -R 9000:localhost:3000 (a port on the server \
         leads here), -D 1080 (SOCKS proxy). Separate several with commas."
    )
    .to_string()
}

/// «Сохранить» доступно только с адресом, который примет ssh, и с
/// правильно записанными пробросами; причина отказа показывается
/// подсказкой у поля.
fn sync_validity(dialog: &adw::AlertDialog, address_row: &adw::EntryRow, forwards_row: &adw::EntryRow) {
    let address_ok = mark_field(address_row, session::validate_target(&address_row.text()).map(drop), None);
    let hint = forwards_hint();
    let forwards_ok = mark_field(forwards_row, forward::parse_list(&forwards_row.text()).map(drop), Some(&hint));
    dialog.set_response_enabled(ACCEPT, address_ok && forwards_ok);
}

/// Подсвечивает поле с ошибкой (если в нём что-то введено) и показывает
/// её подсказкой; без ошибки возвращает обычную подсказку `hint`.
fn mark_field(row: &adw::EntryRow, result: Result<(), String>, hint: Option<&str>) -> bool {
    let ok = result.is_ok();
    match result {
        Err(message) if !row.text().trim().is_empty() => {
            row.add_css_class("error");
            row.set_tooltip_text(Some(&message));
        }
        _ => {
            row.remove_css_class("error");
            row.set_tooltip_text(hint);
        }
    }
    ok
}

/// Заголовок раздела: мелкие прописные буквы (в CSS GTK нет
/// `text-transform`, поэтому регистр меняется здесь).
fn heading_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text.to_uppercase())
        .xalign(0.0)
        .css_classes(["sshdeck-section-title"])
        .build()
}

fn hint_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .margin_bottom(6)
        .css_classes(["caption", "dim-label"])
        .build()
}

fn static_row(label: gtk::Label) -> Row {
    Row::new(
        RowKind::Static,
        gtk::ListBoxRow::builder()
            .child(&label)
            .selectable(false)
            .activatable(false)
            .can_focus(false)
            .build(),
    )
}

impl Row {
    fn new(kind: RowKind, widget: gtk::ListBoxRow) -> Self {
        Self {
            kind,
            widget,
            arrow: None,
            revealer: None,
            status: None,
            root_drop: None,
        }
    }

    fn set_shown(&self, shown: bool, animate: bool) {
        match &self.revealer {
            Some(revealer) => reveal(revealer, &self.widget, shown, animate),
            None => self.widget.set_visible(shown),
        }
    }
}

/// Строка подключения или хоста: значок с точкой доступности, название и
/// адрес мелким шрифтом. Содержимое лежит в `Revealer`, чтобы строку можно
/// было плавно свернуть. `address` — то, что получит ssh, `depth` —
/// уровень вложенности в папки.
fn entry_row(
    title: &str,
    detail: &str,
    address: &str,
    depth: usize,
) -> (gtk::ListBoxRow, gtk::Revealer, StatusView) {
    let labels = gtk::Box::new(gtk::Orientation::Vertical, 0);
    labels.set_valign(gtk::Align::Center);
    labels.append(
        &gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .build(),
    );
    if !detail.is_empty() && detail != title {
        labels.append(
            &gtk::Label::builder()
                .label(detail)
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::End)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.set_margin_top(ROW_MARGIN);
    content.set_margin_bottom(ROW_MARGIN);
    content.set_margin_start(depth as i32 * arrange::INDENT);
    let dot = gtk::Box::builder()
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        .can_target(false)
        .css_classes(["sshdeck-status", "checking"])
        .build();
    let badge = gtk::Overlay::builder()
        .child(&avatar(title))
        .valign(gtk::Align::Center)
        .build();
    badge.add_overlay(&dot);
    content.append(&badge);
    content.append(&labels);
    content.add_css_class("sshdeck-host-content");
    let revealer = collapsing_revealer(gtk::RevealerTransitionType::SlideDown, &content);
    revealer.set_reveal_child(true);
    let row = gtk::ListBoxRow::builder()
        .child(&revealer)
        .css_classes(["sshdeck-host-row"])
        .build();
    let status = StatusView {
        address: address.to_string(),
        dot,
        content,
    };
    (row, revealer, status)
}

/// Строка базы данных: значок СУБД, название и куда подключаемся.
fn database_row(profile: &DbProfile) -> (gtk::ListBoxRow, gtk::Revealer) {
    let labels = gtk::Box::new(gtk::Orientation::Vertical, 0);
    labels.set_valign(gtk::Align::Center);
    labels.append(
        &gtk::Label::builder()
            .label(&profile.name)
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .build(),
    );
    let detail = describe_db(profile);
    if detail != profile.name {
        labels.append(
            &gtk::Label::builder()
                .label(&detail)
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::End)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.set_margin_top(ROW_MARGIN);
    content.set_margin_bottom(ROW_MARGIN);
    content.append(&profile_badge(profile));
    content.append(&labels);
    content.add_css_class("sshdeck-host-content");
    content.set_tooltip_text(Some(&tr!(
        "{} — {}\nDouble-click to connect",
        profile.kind_label(),
        detail
    )));
    let revealer = collapsing_revealer(gtk::RevealerTransitionType::SlideDown, &content);
    revealer.set_reveal_child(true);
    let row = gtk::ListBoxRow::builder()
        .child(&revealer)
        .css_classes(["sshdeck-host-row"])
        .build();
    (row, revealer)
}

/// Сколько цветов у значков-аватаров (`.sshdeck-avatar-0` … в CSS окна).
pub const AVATAR_COLORS: usize = 8;

/// Квадратный значок с первой буквой названия. Цвет выбирается по
/// названию, поэтому у одного и того же сервера он всегда одинаковый.
fn avatar(title: &str) -> gtk::Label {
    let letter: String = title
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "#".to_string());
    let label = gtk::Label::builder()
        .label(letter)
        .valign(gtk::Align::Center)
        .css_classes(["sshdeck-avatar"])
        .build();
    label.add_css_class(&format!("sshdeck-avatar-{}", avatar_color(title)));
    label
}

/// Номер цвета аватара: FNV-1a по названию — устойчиво между запусками,
/// в отличие от `DefaultHasher`.
fn avatar_color(title: &str) -> usize {
    let hash = title.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    hash as usize % AVATAR_COLORS
}

#[cfg(test)]
mod tests {
    use super::{AVATAR_COLORS, avatar_color};

    #[test]
    fn avatar_color_is_stable_and_in_range() {
        for name in [
            "",
            "prod",
            "Прод-веб",
            "testlocal",
            "очень длинное название сервера",
        ] {
            let color = avatar_color(name);
            assert!(color < AVATAR_COLORS, "{name}");
            assert_eq!(color, avatar_color(name), "{name}");
        }
        // FNV-1a: значение зафиксировано, чтобы цвета не менялись между версиями.
        assert_eq!(
            avatar_color("prod"),
            0xed3e_3174_u32 as usize % AVATAR_COLORS
        );
    }
}
