//! Главное окно: боковая панель с хостами и вкладки сессий.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};

use super::hosts::HostsSidebar;
use super::snippets::SnippetsButton;
use super::terminal;
use super::tab::SessionTab;
use super::{Notifier, dialogs};
use crate::session;
use crate::settings::Settings;

const CSS: &str = "
vte-terminal { padding: 6px 8px; }
.sshdeck-files-toolbar > .toolbar + .toolbar { padding-top: 0; }
columnview.sshdeck-files > listview > row > cell { padding-top: 3px; padding-bottom: 3px; }
.sshdeck-transfers { padding: 0 6px 6px 6px; }
.sshdeck-transfer { padding: 6px 6px 10px 12px; }
.sshdeck-address { margin: 4px 8px 2px 8px; }
.sshdeck-section { margin: 10px 14px 2px 14px; }
.sshdeck-note { margin: 6px 14px 10px 14px; }
.sshdeck-hosts > row { min-height: 28px; }
.sshdeck-statusbar { padding: 4px 12px; font-size: 0.9em; border-top: 1px solid alpha(currentColor, 0.12); }
.sshdeck-folder-red { color: #ed333b; }
.sshdeck-folder-orange { color: #ff7800; }
.sshdeck-folder-yellow { color: #e5a50a; }
.sshdeck-folder-green { color: #2ec27e; }
.sshdeck-folder-blue { color: #3584e4; }
.sshdeck-folder-purple { color: #c061cb; }
.sshdeck-folder-brown { color: #b5835a; }
";

const APP_TITLE: &str = "sshdeck";

/// Стандартные темы libadwaita: идентификатор в настройках и название в
/// меню.
const SCHEMES: [(&str, &str); 3] = [
    ("system", "Как в системе"),
    ("light", "Светлая"),
    ("dark", "Тёмная"),
];

/// Шрифт интерфейса, если системный оказался моноширинным.
const UI_FONT_FAMILY: &str = "Adwaita Sans, Inter, Noto Sans, Cantarell, sans-serif";

pub fn build(app: &adw::Application, initial_targets: &[String]) {
    install_css();
    use_proportional_ui_font();
    // Остатки аварийно завершившихся запусков: сокеты и временные файлы.
    if let Ok(dir) = session::runtime_dir() {
        session::cleanup_stale(&dir);
    }
    let window = AppWindow::new(app);
    window.window.present();
    for target in initial_targets {
        window.open_tab(target, None, "");
    }
}

fn install_css() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// Идентификатор темы для меню: цветная тема, если выбрана, иначе
/// стандартное оформление.
fn current_theme_id(settings: &Settings) -> String {
    if terminal::app_look(&settings.terminal_theme).is_some() {
        settings.terminal_theme.clone()
    } else {
        settings.color_scheme.clone()
    }
}

/// Применяет тему ко всему приложению: цвета окна, светлый или тёмный
/// вариант остальных элементов и палитру терминалов.
fn apply_look(provider: &gtk::CssProvider, settings: &Settings) {
    terminal::set_theme(&settings.terminal_theme);
    let style = adw::StyleManager::default();
    match terminal::app_look(&settings.terminal_theme) {
        Some(look) => {
            provider.load_from_string(&look.css);
            style.set_color_scheme(if look.dark {
                adw::ColorScheme::ForceDark
            } else {
                adw::ColorScheme::ForceLight
            });
        }
        None => {
            provider.load_from_string("");
            style.set_color_scheme(match settings.color_scheme.as_str() {
                "light" => adw::ColorScheme::ForceLight,
                "dark" => adw::ColorScheme::ForceDark,
                _ => adw::ColorScheme::Default,
            });
        }
    }
}

/// Если системный шрифт интерфейса моноширинный (как бывает при настройке
/// «один шрифт на всё»), кнопки и списки набираются обычным шрифтом того же
/// размера. Терминал это не затрагивает: его шрифт задаётся отдельно.
fn use_proportional_ui_font() {
    let Some(settings) = gtk::Settings::default() else {
        return;
    };
    let Some(name) = settings.gtk_font_name() else {
        return;
    };
    let mut description = pango::FontDescription::from_string(&name);
    let Some(family) = description.family() else {
        return;
    };
    let context = gtk::Label::new(None).pango_context();
    let monospace = context
        .list_families()
        .iter()
        .any(|candidate| candidate.name().eq_ignore_ascii_case(&family) && candidate.is_monospace());
    if monospace {
        description.set_family(UI_FONT_FAMILY);
        settings.set_gtk_font_name(Some(&description.to_string()));
    }
}

struct AppWindow {
    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    tab_view: adw::TabView,
    tab_bar: adw::TabBar,
    stack: gtk::Stack,
    notifier: Notifier,
    sidebar: HostsSidebar,
    tabs: RefCell<Vec<Rc<SessionTab>>>,
    settings: Rc<RefCell<Settings>>,
    /// Цвета выбранной темы для всего окна.
    theme_css: gtk::CssProvider,
    /// Кнопка сохранённых команд; хранится, чтобы жили её обработчики.
    snippets: SnippetsButton,
    next_tab_id: Cell<u32>,
}

impl AppWindow {
    fn new(app: &adw::Application) -> Rc<Self> {
        // Уведомления окна всплывают над областью вкладок.
        let notifier = Notifier::new();

        // --- боковая панель с подключениями
        let sidebar = HostsSidebar::new(notifier.clone());

        // --- область вкладок
        let tab_view = adw::TabView::new();
        // Alt+цифра и Ctrl+Home/End нужны программам в терминале; для
        // переключения вкладок остаются Ctrl+Tab и Ctrl+PageUp/PageDown.
        tab_view.remove_shortcuts(
            adw::TabViewShortcuts::ALT_DIGITS
                | adw::TabViewShortcuts::ALT_ZERO
                | adw::TabViewShortcuts::CONTROL_HOME
                | adw::TabViewShortcuts::CONTROL_END
                | adw::TabViewShortcuts::CONTROL_SHIFT_HOME
                | adw::TabViewShortcuts::CONTROL_SHIFT_END,
        );
        let tab_bar = adw::TabBar::builder()
            .view(&tab_view)
            .autohide(false)
            .visible(false)
            .build();

        let empty_page = adw::StatusPage::builder()
            .title("Нет открытых сессий")
            .description(
                "Дважды щёлкните подключение слева или введите user@host и нажмите Enter",
            )
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&empty_page, Some("empty"));
        stack.add_named(&tab_view, Some("tabs"));

        notifier.set_child(&stack);

        let settings = Rc::new(RefCell::new(Settings::load()));
        // Цвета выбранной темы поверх стандартных цветов libadwaita.
        let theme_css = gtk::CssProvider::new();
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &theme_css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }
        apply_look(&theme_css, &settings.borrow());
        let snippets = SnippetsButton::new(Rc::clone(&settings), notifier.clone());

        let menu = gio::Menu::new();
        let server_section = gio::Menu::new();
        server_section.append(
            Some("Вход без пароля (ключ) для этой вкладки…"),
            Some("win.key-login"),
        );
        server_section.append(
            Some("Слежение панели за cd на этом сервере…"),
            Some("win.cwd-tracking"),
        );
        menu.append_section(None, &server_section);
        let look_section = gio::Menu::new();
        let theme_menu = gio::Menu::new();
        let standard_themes = gio::Menu::new();
        for (id, label) in SCHEMES {
            standard_themes.append(Some(label), Some(&format!("win.theme::{id}")));
        }
        theme_menu.append_section(None, &standard_themes);
        let color_themes = gio::Menu::new();
        for theme in &terminal::THEMES {
            color_themes.append(Some(theme.name), Some(&format!("win.theme::{}", theme.id)));
        }
        theme_menu.append_section(None, &color_themes);
        look_section.append_submenu(Some("Тема"), &theme_menu);
        look_section.append(Some("Шрифт терминала…"), Some("win.font"));
        menu.append_section(None, &look_section);
        let list_section = gio::Menu::new();
        list_section.append(Some("Перечитать список подключений"), Some("win.reload-hosts"));
        menu.append_section(None, &list_section);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Меню")
            .menu_model(&menu)
            .primary(true)
            .build();
        let sidebar_toggle = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Показать или скрыть список подключений")
            .build();
        let title = adw::WindowTitle::new(APP_TITLE, "");
        let header = adw::HeaderBar::builder().title_widget(&title).build();
        header.pack_start(&sidebar_toggle);
        header.pack_end(&menu_button);
        header.pack_end(snippets.widget());

        let content = adw::ToolbarView::new();
        content.add_top_bar(&header);
        content.add_top_bar(&tab_bar);
        content.set_content(Some(notifier.widget()));

        let split = adw::OverlaySplitView::builder()
            .sidebar(sidebar.widget())
            .content(&content)
            .min_sidebar_width(240.0)
            .max_sidebar_width(320.0)
            .sidebar_width_fraction(0.2)
            .build();
        split
            .bind_property("show-sidebar", &sidebar_toggle, "active")
            .bidirectional()
            .sync_create()
            .build();

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title(APP_TITLE)
            .default_width(1280)
            .default_height(760)
            .content(&split)
            .build();

        let this = Rc::new(Self {
            window,
            title,
            tab_view,
            tab_bar,
            stack,
            notifier,
            sidebar,
            tabs: RefCell::new(Vec::new()),
            settings,
            theme_css,
            snippets,
            next_tab_id: Cell::new(1),
        });

        // Обработчики держат слабые ссылки: окно владеет виджетами, а не
        // наоборот.
        {
            let weak = Rc::downgrade(&this);
            this.sidebar.set_open_handler(move |target, title, startup| {
                weak.upgrade()
                    .is_some_and(|this| this.open_tab(target, title, startup))
            });
        }
        {
            let weak = Rc::downgrade(&this);
            this.snippets.set_run_handler(move |command| {
                let tab = weak.upgrade().and_then(|this| this.current_tab());
                match tab {
                    Some(tab) => {
                        tab.run_command(command);
                        true
                    }
                    None => false,
                }
            });
        }
        {
            let weak = Rc::downgrade(&this);
            let action = gio::SimpleAction::new("cwd-tracking", None);
            action.connect_activate(move |_, _| {
                let Some(this) = weak.upgrade() else { return };
                match this.current_tab() {
                    Some(tab) => tab.setup_cwd_tracking(),
                    None => this
                        .notifier
                        .info("Сначала откройте вкладку и войдите на сервер"),
                }
            });
            this.window.add_action(&action);
        }
        {
            // Тема всего окна вместе с терминалом.
            let weak = Rc::downgrade(&this);
            let current = current_theme_id(&this.settings.borrow());
            let action = gio::SimpleAction::new_stateful(
                "theme",
                Some(glib::VariantTy::STRING),
                &current.to_variant(),
            );
            action.connect_activate(move |action, parameter| {
                let theme = parameter.and_then(|value| value.get::<String>());
                let (Some(this), Some(theme)) = (weak.upgrade(), theme) else {
                    return;
                };
                action.set_state(&theme.to_variant());
                {
                    let mut settings = this.settings.borrow_mut();
                    if SCHEMES.iter().any(|(id, _)| *id == theme) {
                        // Стандартное оформление: цвета libadwaita.
                        settings.color_scheme = theme;
                        settings.terminal_theme = crate::settings::DEFAULT_THEME.to_string();
                    } else {
                        settings.terminal_theme = theme;
                    }
                }
                apply_look(&this.theme_css, &this.settings.borrow());
                for tab in this.tabs.borrow().iter() {
                    tab.refresh_theme();
                }
                this.save_settings();
            });
            this.window.add_action(&action);
        }
        {
            // Закрытие вкладки (крестик, средняя кнопка, штатный выход ssh).
            let weak = Rc::downgrade(&this);
            this.tab_view.connect_close_page(move |view, page| {
                if let Some(this) = weak.upgrade() {
                    let child = page.child();
                    let position = this
                        .tabs
                        .borrow()
                        .iter()
                        .position(|tab| tab.widget().upcast_ref::<gtk::Widget>() == &child);
                    if let Some(position) = position {
                        let tab = this.tabs.borrow_mut().remove(position);
                        tab.shutdown();
                    }
                }
                view.close_page_finish(page, true);
                glib::Propagation::Stop
            });
        }
        {
            let weak = Rc::downgrade(&this);
            this.tab_view.connect_n_pages_notify(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.sync_tabs_state();
                }
            });
        }
        {
            let weak = Rc::downgrade(&this);
            this.tab_view.connect_selected_page_notify(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.sync_tabs_state();
                }
            });
        }
        {
            let weak = Rc::downgrade(&this);
            let action = gio::SimpleAction::new("font", None);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.choose_font();
                }
            });
            this.window.add_action(&action);
        }
        {
            let weak = Rc::downgrade(&this);
            let action = gio::SimpleAction::new("key-login", None);
            action.connect_activate(move |_, _| {
                let Some(this) = weak.upgrade() else { return };
                match this.current_tab() {
                    Some(tab) => tab.setup_key_login(),
                    None => this.notifier.info(
                        "Сначала откройте вкладку и войдите на сервер — ключ ставится через неё",
                    ),
                }
            });
            this.window.add_action(&action);
        }
        {
            let weak = Rc::downgrade(&this);
            let action = gio::SimpleAction::new("reload-hosts", None);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.sidebar.reload();
                }
            });
            this.window.add_action(&action);
        }
        {
            // Замыкание владеет окном: оно живёт, пока существует GTK-окно.
            let this_owned = Rc::clone(&this);
            this.window.connect_close_request(move |_| {
                for tab in this_owned.tabs.borrow_mut().drain(..) {
                    tab.shutdown();
                }
                glib::Propagation::Proceed
            });
        }

        this
    }

    /// Пустой экран или вкладки; в заголовке — адрес текущей сессии.
    fn sync_tabs_state(&self) {
        let has_tabs = self.tab_view.n_pages() > 0;
        self.tab_bar.set_visible(has_tabs);
        self.stack
            .set_visible_child_name(if has_tabs { "tabs" } else { "empty" });
        let subtitle = self
            .tab_view
            .selected_page()
            .map(|page| page.title().to_string())
            .unwrap_or_default();
        self.title.set_subtitle(&subtitle);
    }

    /// Открывает вкладку; `false`, если адрес не прошёл проверку. `title` —
    /// название сохранённого подключения; без него на вкладке виден адрес.
    fn open_tab(self: &Rc<Self>, target: &str, title: Option<&str>, startup: &str) -> bool {
        let target = match session::validate_target(target) {
            Ok(target) => target,
            Err(message) => {
                self.notifier.error(&message);
                return false;
            }
        };
        let runtime_dir: PathBuf = match session::runtime_dir() {
            Ok(dir) => dir,
            Err(err) => {
                dialogs::message(
                    &self.window,
                    "Не удалось подготовить каталог для сокетов",
                    &err.to_string(),
                );
                return false;
            }
        };

        let tab_id = self.next_tab_id.get();
        self.next_tab_id.set(tab_id + 1);

        // Вкладка сообщает о завершении через слот, который заполняется
        // сразу после её создания.
        let slot: Rc<RefCell<Option<std::rc::Weak<SessionTab>>>> = Rc::new(RefCell::new(None));
        let on_finished = {
            let weak_window = Rc::downgrade(self);
            let slot = Rc::clone(&slot);
            move || {
                let tab = slot.borrow().as_ref().and_then(std::rc::Weak::upgrade);
                if let (Some(window), Some(tab)) = (weak_window.upgrade(), tab) {
                    window.close_tab(&tab);
                }
            }
        };
        let font = self.settings.borrow().terminal_font.clone();
        let tab = SessionTab::new(&target, tab_id, &runtime_dir, &font, startup, on_finished);
        *slot.borrow_mut() = Some(Rc::downgrade(&tab));

        self.tabs.borrow_mut().push(Rc::clone(&tab));
        let page = self.tab_view.append(tab.widget());
        match title.filter(|title| *title != tab.target()) {
            Some(title) => {
                page.set_title(title);
                page.set_tooltip(&glib::markup_escape_text(&format!(
                    "{title} — {}",
                    tab.target()
                )));
            }
            None => {
                page.set_title(tab.target());
                page.set_tooltip(&glib::markup_escape_text(tab.target()));
            }
        }
        self.tab_view.set_selected_page(&page);
        self.sync_tabs_state();
        tab.focus_terminal();
        true
    }

    fn save_settings(&self) {
        if let Err(err) = self.settings.borrow().save() {
            self.notifier
                .error(&format!("Не удалось сохранить настройки: {err}"));
        }
    }

    /// Вкладка, которая сейчас на экране.
    fn current_tab(&self) -> Option<Rc<SessionTab>> {
        let child = self.tab_view.selected_page()?.child();
        self.tabs
            .borrow()
            .iter()
            .find(|tab| tab.widget().upcast_ref::<gtk::Widget>() == &child)
            .cloned()
    }

    /// Закрывает вкладку так же, как если бы нажали её крестик.
    fn close_tab(&self, tab: &Rc<SessionTab>) {
        let known = self.tabs.borrow().iter().any(|t| Rc::ptr_eq(t, tab));
        if known {
            self.tab_view
                .close_page(&self.tab_view.page(tab.widget()));
        }
    }

    fn choose_font(self: &Rc<Self>) {
        let current = pango::FontDescription::from_string(&self.settings.borrow().terminal_font);
        let dialog = gtk::FontDialog::builder()
            .title("Шрифт терминала")
            .modal(true)
            .build();
        let weak = Rc::downgrade(self);
        dialog.choose_font(
            Some(&self.window),
            Some(&current),
            gio::Cancellable::NONE,
            move |result| {
                // Ошибка здесь означает, что диалог закрыли без выбора.
                let (Some(this), Ok(description)) = (weak.upgrade(), result) else {
                    return;
                };
                let font = description.to_string();
                for tab in this.tabs.borrow().iter() {
                    tab.set_font(&font);
                }
                this.settings.borrow_mut().terminal_font = font;
                if let Err(err) = this.settings.borrow().save() {
                    this.notifier
                        .error(&format!("Не удалось сохранить настройки: {err}"));
                }
            },
        );
    }
}

