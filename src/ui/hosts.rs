//! Боковая панель: быстрое подключение по адресу, сохранённые подключения
//! (с названиями и папками) и хосты из ssh-конфига.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};

use super::{Notifier, dialogs};
use crate::connections::{self, Connection, Connections, FOLDER_COLORS};
use crate::session;
use crate::ssh_config::{self, HostEntry};

const CANCEL: &str = "cancel";
const ACCEPT: &str = "accept";

const ICON_EXPANDED: &str = "pan-down-symbolic";
const ICON_COLLAPSED: &str = "pan-end-symbolic";

/// Поля над и под содержимым строки списка.
const ROW_MARGIN: i32 = 2;

/// Открывает вкладку: адрес, необязательное название и команда после входа. Возвращает `false`,
/// если адрес не подошёл.
type OpenHandler = Box<dyn Fn(&str, Option<&str>, &str) -> bool>;

#[derive(Clone)]
enum RowKind {
    /// Заголовок раздела или подсказка: не выбирается.
    Static,
    Folder(String),
    /// Индекс в списке сохранённых подключений.
    Connection(usize),
    /// Индекс в списке хостов из ssh-конфига.
    Host(usize),
}

struct Row {
    kind: RowKind,
    widget: gtk::ListBoxRow,
    /// Стрелка «свёрнуто/развёрнуто» у строки папки.
    arrow: Option<gtk::Image>,
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
    collapsed: RefCell<HashSet<String>>,
    /// Строка, для которой открыто контекстное меню.
    menu_row: Cell<Option<usize>>,
    open: RefCell<Option<OpenHandler>>,
}

impl HostsSidebar {
    pub fn new(notifier: Notifier) -> Self {
        let entry = gtk::Entry::builder()
            .placeholder_text("Поиск или user@host")
            .tooltip_text(
                "Быстрое подключение: user@host, user@host -p 2222 \
                 или псевдоним из ~/.ssh/config",
            )
            .primary_icon_name("system-search-symbolic")
            .secondary_icon_name("go-next-symbolic")
            .secondary_icon_tooltip_text("Подключиться")
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
            .tooltip_text("Сохранить новое подключение")
            .action_name("hosts.new")
            .css_classes(["flat"])
            .build();
        let reload_button = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Перечитать сохранённые подключения и ~/.ssh/config")
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

        let menu = gtk::PopoverMenu::from_model(gio::MenuModel::NONE);
        menu.set_parent(&scroll);
        menu.set_has_arrow(false);
        menu.set_halign(gtk::Align::Start);
        {
            let menu = menu.clone();
            scroll.connect_destroy(move |_| menu.unparent());
        }

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
            collapsed: RefCell::new(HashSet::new()),
            menu_row: Cell::new(None),
            open: RefCell::new(None),
        });
        inner.connect_signals();
        inner.reload();
        Self { inner }
    }

    pub fn widget(&self) -> &adw::ToolbarView {
        &self.inner.root
    }

    /// Задаёт способ открыть вкладку; без него панель ничего не открывает.
    pub fn set_open_handler(
        &self,
        handler: impl Fn(&str, Option<&str>, &str) -> bool + 'static,
    ) {
        *self.inner.open.borrow_mut() = Some(Box::new(handler));
    }

    /// Перечитывает сохранённые подключения и ssh-конфиг.
    pub fn reload(&self) {
        self.inner.reload();
    }
}

impl Inner {
    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);

        {
            let weak = weak.clone();
            let open = move || {
                if let Some(this) = weak.upgrade()
                    && this.open(&this.entry.text(), None, "")
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
                    this.apply_collapsed();
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

        let actions = gio::SimpleActionGroup::new();
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
            this.edit_dialog("", &address, "", "", None);
        });
        add("connect", |this| {
            if let Some(kind) = this.menu_kind() {
                this.activate(&kind);
            }
        });
        add("edit", |this| {
            if let Some(c) = this.menu_connection() {
                this.edit_dialog(&c.name, &c.address, &c.folder, &c.startup, Some(c.clone()));
            }
        });
        add("duplicate", |this| {
            if let Some(c) = this.menu_connection() {
                let name = format!("{} (копия)", c.name);
                this.edit_dialog(&name, &c.address, &c.folder, &c.startup, None);
            }
        });
        add("delete", |this| this.delete_connection());
        add("save-host", |this| {
            let alias = match this.menu_kind() {
                Some(RowKind::Host(index)) => {
                    this.hosts.borrow().get(index).map(|h| h.alias.clone())
                }
                _ => None,
            };
            if let Some(alias) = alias {
                this.edit_dialog(&alias, &alias, "", "", None);
            }
        });
        add("new-in-folder", |this| {
            if let Some(folder) = this.menu_folder() {
                this.edit_dialog("", "", &folder, "", None);
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
        self.root.insert_action_group("hosts", Some(&actions));
    }

    fn open(&self, target: &str, title: Option<&str>, startup: &str) -> bool {
        match self.open.borrow().as_ref() {
            Some(open) => open(target, title, startup),
            None => false,
        }
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
            Ok(list) => *self.connections.borrow_mut() = list,
            Err(err) => {
                // Повреждённый файл откладываем в сторону, чтобы следующее
                // сохранение не затёрло то, что в нём ещё можно прочитать.
                let mut aside = self.path.as_os_str().to_owned();
                aside.push(".broken");
                let aside = PathBuf::from(aside);
                let moved = std::fs::rename(&self.path, &aside).is_ok();
                *self.connections.borrow_mut() = Connections::default();
                let tail = if moved {
                    format!("Файл переименован в {}.", aside.display())
                } else {
                    "Исправьте файл вручную: новое сохранение его перезапишет.".to_string()
                };
                self.notifier.error(&format!(
                    "Не удалось прочитать сохранённые подключения: {err}\n{tail}"
                ));
            }
        }
        self.rebuild();
    }

    /// Записывает список на диск и перерисовывает панель.
    fn commit(&self) {
        if let Err(err) = self.connections.borrow().save(&self.path) {
            self.notifier
                .error(&format!("Не удалось сохранить подключения: {err}"));
        }
        self.rebuild();
    }

    fn store(&self, connection: Connection, original: Option<Connection>) {
        self.collapsed.borrow_mut().remove(&connection.folder);
        {
            let mut list = self.connections.borrow_mut();
            // Запись ищется по содержимому: индекс мог устареть.
            let position = original
                .as_ref()
                .and_then(|original| list.items().iter().position(|c| c == original));
            match position {
                Some(index) => {
                    list.replace(index, connection);
                }
                None => list.add(connection),
            }
        }
        self.commit();
    }

    // ----- список

    fn rebuild(&self) {
        self.rows.borrow_mut().clear();
        self.list.remove_all();

        let mut rows = Vec::new();
        {
            let connections = self.connections.borrow();
            rows.push(static_row(heading_label("Сохранённые")));
            if connections.items().is_empty() {
                rows.push(static_row(hint_label(
                    "Пока пусто. Нажмите «+» вверху, чтобы сохранить подключение \
                     с названием и папкой.",
                )));
            }
            let mut current_folder = "";
            for (index, item) in connections.items().iter().enumerate() {
                if item.folder != current_folder {
                    current_folder = &item.folder;
                    let count = connections
                        .items()
                        .iter()
                        .filter(|c| c.folder == item.folder)
                        .count();
                    rows.push(self.folder_row(
                        &item.folder,
                        count,
                        connections.folder_color(&item.folder),
                    ));
                }
                rows.push(Row {
                    kind: RowKind::Connection(index),
                    widget: entry_row(&item.name, &item.address, !item.folder.is_empty()),
                    arrow: None,
                });
            }

            let hosts = self.hosts.borrow();
            if !hosts.is_empty() {
                rows.push(static_row(heading_label("Из ~/.ssh/config")));
            }
            for (index, host) in hosts.iter().enumerate() {
                rows.push(Row {
                    kind: RowKind::Host(index),
                    widget: entry_row(&host.alias, &host.detail(), false),
                    arrow: None,
                });
            }
        }
        for row in &rows {
            self.list.append(&row.widget);
        }
        *self.rows.borrow_mut() = rows;
        self.apply_collapsed();
    }

    fn folder_row(&self, folder: &str, count: usize, color: Option<&str>) -> Row {
        let arrow = gtk::Image::from_icon_name(ICON_EXPANDED);
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(ROW_MARGIN);
        content.set_margin_bottom(ROW_MARGIN);
        content.append(&arrow);
        let icon = gtk::Image::from_icon_name("folder-symbolic");
        let name_label = gtk::Label::builder()
            .label(folder)
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
        let widget = gtk::ListBoxRow::builder()
            .child(&content)
            .tooltip_text("Щелчок сворачивает и разворачивает папку")
            .build();

        let gesture = gtk::GestureClick::builder()
            .button(gdk::BUTTON_PRIMARY)
            .build();
        let weak = self.me.clone();
        let name = folder.to_string();
        gesture.connect_released(move |_, n_press, _, _| {
            // Второй щелчок двойного нажатия приходит как активация строки.
            if n_press == 1
                && let Some(this) = weak.upgrade()
            {
                this.toggle_folder(&name);
            }
        });
        widget.add_controller(gesture);

        Row {
            kind: RowKind::Folder(folder.to_string()),
            widget,
            arrow: Some(arrow),
        }
    }

    fn toggle_folder(&self, folder: &str) {
        {
            let mut collapsed = self.collapsed.borrow_mut();
            if !collapsed.remove(folder) {
                collapsed.insert(folder.to_string());
            }
        }
        self.apply_collapsed();
    }

    /// Показывает и прячет строки: свёрнутые папки и поиск по тексту,
    /// набранному в поле адреса.
    fn apply_collapsed(&self) {
        let needle = self.entry.text().trim().to_lowercase();
        let matches = |title: &str, detail: &str| {
            needle.is_empty()
                || title.to_lowercase().contains(&needle)
                || detail.to_lowercase().contains(&needle)
        };
        let collapsed = self.collapsed.borrow();
        let connections = self.connections.borrow();
        let hosts = self.hosts.borrow();
        for row in self.rows.borrow().iter() {
            match &row.kind {
                RowKind::Folder(folder) => {
                    if let Some(arrow) = &row.arrow {
                        arrow.set_icon_name(Some(if collapsed.contains(folder) {
                            ICON_COLLAPSED
                        } else {
                            ICON_EXPANDED
                        }));
                    }
                    // При поиске папка видна, только если в ней есть находки.
                    let any = connections
                        .items()
                        .iter()
                        .any(|c| &c.folder == folder && matches(&c.name, &c.address));
                    row.widget.set_visible(any || matches(folder, ""));
                }
                RowKind::Connection(index) => {
                    let visible = connections.get(*index).is_some_and(|c| {
                        if needle.is_empty() {
                            !collapsed.contains(&c.folder)
                        } else {
                            matches(&c.name, &c.address) || matches(&c.folder, "")
                        }
                    });
                    row.widget.set_visible(visible);
                }
                RowKind::Host(index) => {
                    let visible = hosts
                        .get(*index)
                        .is_some_and(|h| matches(&h.alias, &h.detail()));
                    row.widget.set_visible(visible);
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
            RowKind::Connection(index) => {
                let connection = self.connections.borrow().get(*index).cloned();
                if let Some(c) = connection {
                    self.open(&c.address, Some(&c.name), &c.startup);
                }
            }
            RowKind::Host(index) => {
                let alias = self.hosts.borrow().get(*index).map(|h| h.alias.clone());
                if let Some(alias) = alias {
                    self.open(&alias, None, "");
                }
            }
            RowKind::Folder(folder) => self.toggle_folder(folder),
            RowKind::Static => {}
        }
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
        match kind {
            RowKind::Connection(_) => {
                section(&[
                    ("Подключиться", "hosts.connect"),
                    ("Изменить…", "hosts.edit"),
                    ("Дублировать…", "hosts.duplicate"),
                ]);
                section(&[("Удалить…", "hosts.delete")]);
            }
            RowKind::Folder(_) => {
                section(&[
                    ("Новое подключение в папке…", "hosts.new-in-folder"),
                    ("Переименовать папку…", "hosts.rename-folder"),
                ]);
                let colors = gio::Menu::new();
                for (id, label) in FOLDER_COLORS {
                    colors.append(Some(label), Some(&format!("hosts.folder-color::{id}")));
                }
                colors.append(Some("Без цвета"), Some("hosts.folder-color::"));
                menu.append_section(Some("Цвет папки"), &colors);
                section(&[
                    ("Убрать папку, оставив подключения", "hosts.dissolve-folder"),
                    ("Удалить папку с подключениями…", "hosts.delete-folder"),
                ]);
            }
            RowKind::Host(_) => section(&[
                ("Подключиться", "hosts.connect"),
                ("Сохранить в подключения…", "hosts.save-host"),
            ]),
            RowKind::Static => section(&[("Новое подключение…", "hosts.new")]),
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

    fn menu_kind(&self) -> Option<RowKind> {
        let index = self.menu_row.get()?;
        self.rows.borrow().get(index).map(|row| row.kind.clone())
    }

    fn menu_connection(&self) -> Option<Connection> {
        match self.menu_kind()? {
            RowKind::Connection(index) => self.connections.borrow().get(index).cloned(),
            _ => None,
        }
    }

    fn menu_folder(&self) -> Option<String> {
        match self.menu_kind()? {
            RowKind::Folder(folder) => Some(folder),
            _ => None,
        }
    }

    // ----- действия

    fn delete_connection(self: &Rc<Self>) {
        let Some(connection) = self.menu_connection() else {
            return;
        };
        let weak = Rc::downgrade(self);
        dialogs::confirm(
            &self.root,
            "Удалить подключение?",
            &format!(
                "«{}» ({}) исчезнет из списка. На сервере ничего не меняется.",
                connection.name, connection.address
            ),
            "Удалить",
            true,
            move || {
                let Some(this) = weak.upgrade() else { return };
                {
                    let mut list = this.connections.borrow_mut();
                    if let Some(index) = list.items().iter().position(|c| c == &connection) {
                        list.remove(index);
                    }
                }
                this.commit();
            },
        );
    }

    fn rename_folder(self: &Rc<Self>) {
        let Some(folder) = self.menu_folder() else {
            return;
        };
        let weak = Rc::downgrade(self);
        dialogs::prompt_text(
            &self.root,
            "Переименовать папку",
            "Если папка с таким названием уже есть, подключения объединятся в ней.",
            &folder.clone(),
            "Переименовать",
            move |name| {
                let Some(this) = weak.upgrade() else { return };
                let name = connections::clean(&name);
                if name.is_empty() || name == folder {
                    return;
                }
                this.connections.borrow_mut().move_folder(&folder, &name);
                {
                    let mut collapsed = this.collapsed.borrow_mut();
                    if collapsed.remove(&folder) {
                        collapsed.insert(name);
                    }
                }
                this.commit();
            },
        );
    }

    fn set_folder_color(&self, color: &str) {
        let Some(folder) = self.menu_folder() else {
            return;
        };
        self.connections
            .borrow_mut()
            .set_folder_color(&folder, Some(color).filter(|color| !color.is_empty()));
        self.commit();
    }

    fn dissolve_folder(&self) {
        let Some(folder) = self.menu_folder() else {
            return;
        };
        self.connections.borrow_mut().move_folder(&folder, "");
        self.collapsed.borrow_mut().remove(&folder);
        self.commit();
        self.notifier
            .info(&format!("Папка «{folder}» убрана, подключения сохранены"));
    }

    fn delete_folder(self: &Rc<Self>) {
        let Some(folder) = self.menu_folder() else {
            return;
        };
        let count = self
            .connections
            .borrow()
            .items()
            .iter()
            .filter(|c| c.folder == folder)
            .count();
        let weak = Rc::downgrade(self);
        dialogs::confirm(
            &self.root,
            "Удалить папку с подключениями?",
            &format!(
                "Папка «{folder}» и сохранённые в ней подключения ({count}) исчезнут из \
                 списка. На серверах ничего не меняется."
            ),
            "Удалить",
            true,
            move || {
                let Some(this) = weak.upgrade() else { return };
                {
                    let mut list = this.connections.borrow_mut();
                    while let Some(index) = list.items().iter().position(|c| c.folder == folder) {
                        list.remove(index);
                    }
                }
                this.collapsed.borrow_mut().remove(&folder);
                this.commit();
            },
        );
    }

    /// Диалог создания (`original == None`) или изменения подключения.
    fn edit_dialog(
        self: &Rc<Self>,
        name: &str,
        address: &str,
        folder: &str,
        startup: &str,
        original: Option<Connection>,
    ) {
        let name_row = adw::EntryRow::builder()
            .title("Название")
            .text(name)
            .activates_default(true)
            .build();
        let address_row = adw::EntryRow::builder()
            .title("Адрес: user@host или user@host -p 2222")
            .text(address)
            .activates_default(true)
            .build();
        let folder_row = adw::EntryRow::builder()
            .title("Папка (необязательно)")
            .text(folder)
            .activates_default(true)
            .build();

        let folders = self.connections.borrow().folders();
        if !folders.is_empty() {
            let choices = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let popover = gtk::Popover::builder().child(&choices).build();
            for existing in folders {
                let label = gtk::Label::builder()
                    .label(&existing)
                    .xalign(0.0)
                    .ellipsize(pango::EllipsizeMode::End)
                    .max_width_chars(30)
                    .build();
                let button = gtk::Button::builder()
                    .child(&label)
                    .css_classes(["flat"])
                    .build();
                let row = folder_row.downgrade();
                let popover = popover.downgrade();
                button.connect_clicked(move |_| {
                    if let Some(row) = row.upgrade() {
                        row.set_text(&existing);
                    }
                    if let Some(popover) = popover.upgrade() {
                        popover.popdown();
                    }
                });
                choices.append(&button);
            }
            folder_row.add_suffix(
                &gtk::MenuButton::builder()
                    .icon_name("folder-symbolic")
                    .tooltip_text("Выбрать существующую папку")
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
            .title("Команда после входа (необязательно)")
            .text(startup)
            .activates_default(true)
            .build();
        fields.append(&startup_row);

        let heading = if original.is_some() {
            "Изменить подключение"
        } else {
            "Новое подключение"
        };
        let dialog = adw::AlertDialog::new(
            Some(heading),
            Some("Пароли не сохраняются: их по-прежнему спрашивает ssh в терминале."),
        );
        dialog.add_responses(&[(CANCEL, "Отмена"), (ACCEPT, "Сохранить")]);
        dialog.set_close_response(CANCEL);
        dialog.set_default_response(Some(ACCEPT));
        dialog.set_response_appearance(ACCEPT, adw::ResponseAppearance::Suggested);
        dialog.set_extra_child(Some(&fields));
        dialog.set_focus(Some(if address.trim().is_empty() && !name.trim().is_empty() {
            &address_row
        } else {
            &name_row
        }));

        sync_validity(&dialog, &address_row);
        let weak_dialog = dialog.downgrade();
        address_row.connect_changed(move |row| {
            if let Some(dialog) = weak_dialog.upgrade() {
                sync_validity(&dialog, row);
            }
        });

        let weak = Rc::downgrade(self);
        dialog.choose(Some(&self.root), gio::Cancellable::NONE, move |response| {
            if response != ACCEPT {
                return;
            }
            let Some(this) = weak.upgrade() else { return };
            let connection = Connection::new(&name_row.text(), &address_row.text(), &folder_row.text())
                .with_startup(&startup_row.text());
            if session::validate_target(&connection.address).is_ok() {
                this.store(connection, original);
            }
        });
    }
}

/// «Сохранить» доступно только с адресом, который примет ssh; причина
/// отказа показывается подсказкой у поля.
fn sync_validity(dialog: &adw::AlertDialog, address_row: &adw::EntryRow) {
    let text = address_row.text();
    let result = session::validate_target(&text);
    dialog.set_response_enabled(ACCEPT, result.is_ok());
    match result {
        Err(message) if !text.trim().is_empty() => {
            address_row.add_css_class("error");
            address_row.set_tooltip_text(Some(&message));
        }
        _ => {
            address_row.remove_css_class("error");
            address_row.set_tooltip_text(None);
        }
    }
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
    Row {
        kind: RowKind::Static,
        widget: gtk::ListBoxRow::builder()
            .child(&label)
            .selectable(false)
            .activatable(false)
            .can_focus(false)
            .build(),
        arrow: None,
    }
}

/// Строка подключения или хоста: значок, название и адрес мелким шрифтом.
fn entry_row(title: &str, detail: &str, indented: bool) -> gtk::ListBoxRow {
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
    if indented {
        content.set_margin_start(20);
    }
    content.append(&avatar(title));
    content.append(&labels);
    gtk::ListBoxRow::builder()
        .child(&content)
        .build()
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
