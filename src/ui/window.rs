//! Главное окно: боковая панель с хостами и вкладки сессий.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};

use super::databases::{Databases, describe};
use super::dbtab::{ControlLookup, DbTab};
use super::hosts::HostsSidebar;
use super::snippets::SnippetsButton;
use super::terminal;
use super::tab::SessionTab;
use super::{ANIMATION_MS, Notifier, collapsing_revealer, dialogs, reveal};
use crate::db::DbProfile;
use crate::session;
use crate::settings::Settings;

/// Собственный облик поверх libadwaita. Цвета берутся из переменных
/// libadwaita (`--window-bg-color` и т. п.), которые задаёт выбранная тема,
/// поэтому правила одинаково работают в светлом и тёмном вариантах.
const CSS: &str = "
/* ---------- header bars: tabs right in the window title bar */
.sshdeck-topbar, .sshdeck-sidebar-header { min-height: 40px; padding: 0 6px; }
.sshdeck-sidebar-header { padding-left: 14px; }
.sshdeck-topbar {
  background-color: var(--headerbar-bg-color);
  color: var(--headerbar-fg-color);
  box-shadow: inset 0 -1px color-mix(in srgb, currentColor 8%, transparent);
}
.sshdeck-topbar > button, .sshdeck-topbar > menubutton > button,
.sshdeck-sidebar-header > button {
  min-width: 28px; min-height: 28px; padding: 0; border-radius: 7px;
}
.sshdeck-topbar > revealer > button { min-width: 28px; min-height: 28px; padding: 0; border-radius: 7px; }
.sshdeck-topbar > button.toggle:checked, .sshdeck-topbar > revealer > button.toggle:checked {
  background-color: color-mix(in srgb, currentColor 10%, transparent);
  box-shadow: none;
}
.sshdeck-topbar > button.toggle:checked:hover,
.sshdeck-topbar > revealer > button.toggle:checked:hover {
  background-color: color-mix(in srgb, currentColor 15%, transparent);
}
.sshdeck-topbar windowcontrols > button > image,
.sshdeck-sidebar-header windowcontrols > button > image {
  background: none; border-radius: 7px; padding: 5px;
}
.sshdeck-topbar windowcontrols > button:hover > image,
.sshdeck-sidebar-header windowcontrols > button:hover > image {
  background-color: color-mix(in srgb, currentColor 10%, transparent);
}
.sshdeck-topbar windowcontrols > button.close:hover > image,
.sshdeck-sidebar-header windowcontrols > button.close:hover > image {
  background-color: #e5484d; color: #ffffff;
}

.sshdeck-topbar tabbar .box { background: none; box-shadow: none; padding: 0; }
.sshdeck-topbar tabbar tabbox { padding-top: 0; padding-bottom: 0; min-height: 40px; }
.sshdeck-topbar tabbar tabbox > separator { opacity: 0; }
.sshdeck-topbar tabbar tab {
  min-height: 22px; margin: 5px 0; padding: 2px 3px 2px 10px; border-radius: 8px;
  color: color-mix(in srgb, currentColor 60%, transparent);
}
.sshdeck-topbar tabbar tab:hover { color: var(--headerbar-fg-color); }
.sshdeck-topbar tabbar tab:selected,
.sshdeck-topbar tabbar tabbox.single-tab tab:selected {
  background-color: var(--view-bg-color);
  color: var(--view-fg-color);
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.22),
              inset 0 0 0 1px color-mix(in srgb, var(--view-fg-color) 7%, transparent);
}
.sshdeck-topbar tabbar tab:selected label { font-weight: 600; }

/* ---------- sidebar */
.sshdeck-logo {
  min-width: 22px; min-height: 22px; margin-right: 6px; border-radius: 7px;
  background-color: var(--accent-bg-color); color: var(--accent-fg-color);
  font-family: monospace; font-weight: 800; font-size: 0.85em;
}
.sshdeck-brand { font-weight: 800; font-size: 1.08em; }
.sshdeck-address { margin: 2px 10px 6px 10px; min-height: 30px; border-radius: 8px; }
.sshdeck-section-title {
  margin: 8px 4px 2px 4px; font-size: 0.7em; font-weight: 800; letter-spacing: 0.1em;
  color: color-mix(in srgb, currentColor 48%, transparent);
}
.sshdeck-note { margin: 6px 14px 10px 14px; }
.sshdeck-hosts { background: none; }
.sshdeck-hosts > row { margin: 0 6px 1px 6px; border-radius: 8px; }
.sshdeck-hosts > row.activatable { min-height: 32px; }
.sshdeck-hosts > row.sshdeck-host-row { min-height: 0; padding-top: 0; padding-bottom: 0; }
.sshdeck-host-content { min-height: 28px; }
.sshdeck-hosts > row:selected {
  background-color: color-mix(in srgb, var(--accent-bg-color) 22%, transparent);
}
.sshdeck-hosts > row:selected:hover {
  background-color: color-mix(in srgb, var(--accent-bg-color) 28%, transparent);
}
.sshdeck-avatar {
  min-width: 22px; min-height: 22px; border-radius: 6px;
  font-weight: 800; font-size: 0.78em;
}
.sshdeck-avatar-0 { color: #5b7cff; background-color: color-mix(in srgb, #5b7cff 18%, transparent); }
.sshdeck-avatar-1 { color: #9b6dff; background-color: color-mix(in srgb, #9b6dff 18%, transparent); }
.sshdeck-avatar-2 { color: #e5559a; background-color: color-mix(in srgb, #e5559a 18%, transparent); }
.sshdeck-avatar-3 { color: #e5484d; background-color: color-mix(in srgb, #e5484d 18%, transparent); }
.sshdeck-avatar-4 { color: #e8803a; background-color: color-mix(in srgb, #e8803a 18%, transparent); }
.sshdeck-avatar-5 { color: #c9921a; background-color: color-mix(in srgb, #c9921a 18%, transparent); }
.sshdeck-avatar-6 { color: #2bb062; background-color: color-mix(in srgb, #2bb062 18%, transparent); }
.sshdeck-avatar-7 { color: #14a3b8; background-color: color-mix(in srgb, #14a3b8 18%, transparent); }

/* ---------- empty page */
.sshdeck-empty > scrolledwindow > viewport > box > clamp > box > .icon {
  color: var(--accent-color); -gtk-icon-size: 96px;
}
.sshdeck-empty .title { font-weight: 800; }

/* ---------- tab: files | terminal */
paned.sshdeck-split > separator {
  background-color: color-mix(in srgb, currentColor 9%, transparent);
}
vte-terminal { padding: 4px 8px; }
.sshdeck-files-toolbar { padding: 2px 2px 0 2px; }
.sshdeck-files-toolbar > .toolbar + .toolbar { padding-top: 0; }
.sshdeck-files-toolbar button { border-radius: 8px; }
.sshdeck-files-toolbar entry {
  min-height: 26px; border-radius: 7px; font-family: monospace; font-size: 0.92em;
}
columnview.sshdeck-files > header > button { padding-top: 3px; padding-bottom: 3px; }
columnview.sshdeck-files > header > button > box { font-size: 0.78em; font-weight: 700; }
columnview.sshdeck-files > listview > row > cell { padding-top: 1px; padding-bottom: 1px; }
columnview.sshdeck-files > listview > row:selected {
  background-color: color-mix(in srgb, var(--accent-bg-color) 22%, transparent);
}
.sshdeck-transfers { padding: 0 6px 6px 6px; }
.sshdeck-transfer { padding: 6px 6px 10px 12px; }
.sshdeck-section { margin: 10px 14px 2px 14px; }

/* ---------- server availability: a dot in the corner of the icon. Colors are in sRGB:
   libadwaita computes --success-color in oklab, and it does not render correctly everywhere */
.sshdeck-status {
  min-width: 8px; min-height: 8px; border-radius: 99px;
  margin: 0 -2px -2px 0;
  box-shadow: 0 0 0 2px var(--sidebar-bg-color);
  background-color: color-mix(in srgb, currentColor 35%, transparent);
  transition: background-color 300ms ease-out;
}
.sshdeck-status.online { background-color: #2ec27e; }
.sshdeck-status.offline { background-color: #e5484d; }
.sshdeck-status.checking { animation: sshdeck-pulse 1.4s ease-in-out infinite; }
@keyframes sshdeck-pulse { 0% { opacity: 0.35; } 50% { opacity: 1; } 100% { opacity: 0.35; } }
.sshdeck-host-content.offline .sshdeck-avatar { opacity: 0.45; }
.sshdeck-host-content > box > label { transition: opacity 300ms ease-out; }
.sshdeck-host-content.offline > box > label { opacity: 0.6; }

/* ---------- smooth transitions (GTK disables them when animations are off) */
.sshdeck-hosts > row, .sshdeck-topbar tabbar tab, .sshdeck-topbar > button,
.sshdeck-topbar > revealer > button, .sshdeck-topbar > menubutton > button,
columnview.sshdeck-files > listview > row, .sshdeck-stat {
  transition: background-color 160ms ease-out, color 160ms ease-out, box-shadow 160ms ease-out;
}
.sshdeck-folder-arrow { transition: transform 200ms cubic-bezier(0.25, 0.46, 0.45, 0.94); }
.sshdeck-folder-arrow.collapsed { transform: rotate(-90deg); }

/* ---------- server status bar */
.sshdeck-statusbar {
  padding: 2px 8px; font-size: 0.85em;
  background-color: var(--window-bg-color);
  box-shadow: inset 0 1px color-mix(in srgb, currentColor 8%, transparent);
}
.sshdeck-stat {
  padding: 1px 8px; border-radius: 99px;
  background-color: color-mix(in srgb, currentColor 6%, transparent);
}

/* ---------- databases */
.sshdeck-db-badge {
  min-width: 22px; min-height: 22px; border-radius: 6px;
  font-weight: 800; font-size: 0.7em; font-family: monospace;
}
.sshdeck-db-postgres { color: #4f7fd6; background-color: color-mix(in srgb, #4f7fd6 18%, transparent); }
.sshdeck-db-mysql { color: #e8803a; background-color: color-mix(in srgb, #e8803a 18%, transparent); }
.sshdeck-db-sqlite { color: #14a3b8; background-color: color-mix(in srgb, #14a3b8 18%, transparent); }
.sshdeck-db-toolbar {
  padding: 3px 6px; min-height: 30px;
  box-shadow: inset 0 -1px color-mix(in srgb, currentColor 8%, transparent);
}
.sshdeck-db-toolbar button { border-radius: 7px; min-height: 26px; }
.sshdeck-db-toolbar entry, .sshdeck-db-toolbar searchentry { min-height: 26px; border-radius: 7px; }
.sshdeck-db-nav > row { min-height: 0; padding-top: 2px; padding-bottom: 2px; }
.sshdeck-db-extension { color: #9b6bd6; background-color: color-mix(in srgb, #9b6bd6 18%, transparent); }
.sshdeck-db-active { font-weight: bold; }
.sshdeck-tree-size { font-size: 0.85em; font-feature-settings: 'tnum'; }
.sshdeck-tree-note { font-style: italic; }
.sshdeck-tree-pk { color: #d0a12e; }
textview.sshdeck-sql > text { background-color: var(--view-bg-color); }
textview.sshdeck-sql { font-family: monospace; }
.sshdeck-db-caption { font-family: monospace; font-size: 0.8em; }
/* result grid */
columnview.sshdeck-grid { background-color: var(--view-bg-color); }
columnview.sshdeck-grid > header > button {
  padding: 6px 10px;
  background-color: color-mix(in srgb, currentColor 5%, var(--view-bg-color));
  box-shadow: inset 0 -1px color-mix(in srgb, currentColor 12%, transparent);
}
columnview.sshdeck-grid > header > button > box {
  font-size: 0.82em; font-weight: 700;
  color: color-mix(in srgb, currentColor 75%, transparent);
}
columnview.sshdeck-grid > listview > row > cell { padding: 0; }
columnview.sshdeck-grid > listview > row:hover {
  background-color: color-mix(in srgb, var(--accent-bg-color) 8%, transparent);
}
columnview.sshdeck-grid > listview > row:selected {
  background-color: color-mix(in srgb, var(--accent-bg-color) 26%, transparent);
}
columnview.sshdeck-grid.column-separators > listview > row > cell,
columnview.sshdeck-grid.column-separators > header > button {
  border-right: 1px solid color-mix(in srgb, currentColor 7%, transparent);
}
.sshdeck-cell { padding: 5px 10px; min-height: 18px; }
.sshdeck-cell.odd { background-color: color-mix(in srgb, currentColor 3.5%, transparent); }
.sshdeck-cell.numeric { font-feature-settings: 'tnum'; }
.sshdeck-rownum {
  font-size: 0.8em; padding: 5px 8px 5px 12px;
  color: color-mix(in srgb, currentColor 38%, transparent);
}
.sshdeck-null {
  font-size: 0.72em; font-weight: 700; letter-spacing: 0.06em;
  color: color-mix(in srgb, currentColor 35%, transparent);
}
.sshdeck-results-header { min-height: 34px; }
.sshdeck-pill {
  padding: 1px 9px; border-radius: 99px; font-size: 0.8em; font-weight: 600;
  background-color: color-mix(in srgb, currentColor 8%, transparent);
  color: color-mix(in srgb, currentColor 80%, transparent);
}
.sshdeck-message { padding: 14px 16px; border-radius: 12px; }
.sshdeck-message.error {
  background-color: color-mix(in srgb, #e5484d 12%, transparent);
  box-shadow: inset 0 0 0 1px color-mix(in srgb, #e5484d 30%, transparent);
}
.sshdeck-message.error > image, .sshdeck-message.error .heading { color: #e5484d; }
.sshdeck-message.success {
  background-color: color-mix(in srgb, #2ec27e 12%, transparent);
  box-shadow: inset 0 0 0 1px color-mix(in srgb, #2ec27e 30%, transparent);
}
.sshdeck-message.success > image { color: #2ec27e; }
.sshdeck-message-text { font-family: monospace; font-size: 0.92em; }
.sshdeck-editor-hint {
  font-family: monospace; color: color-mix(in srgb, currentColor 32%, transparent);
}
.sshdeck-section-button { min-width: 22px; min-height: 22px; padding: 0; margin: 6px 6px 0 0; border-radius: 6px; }

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
    ("system", n_!("System default")),
    ("light", n_!("Light")),
    ("dark", n_!("Dark")),
];

/// Шрифт интерфейса, если системный оказался моноширинным.
const UI_FONT_FAMILY: &str = "Adwaita Sans, Inter, Noto Sans, Cantarell, sans-serif";
/// Во сколько раз шрифт интерфейса мельче системного.
const UI_FONT_SCALE: f64 = 0.9;

pub fn build(app: &adw::Application, initial_targets: &[String]) {
    install_css();
    tune_ui_font();
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

/// Значок приложения для пустого экрана, растрированный с запасом (256 px),
/// чтобы оставаться чётким на экранах с масштабом 2. `None`, если в системе
/// нет загрузчика SVG.
fn app_icon() -> Option<gdk::Texture> {
    let svg = include_str!("../../data/dev.sshdeck.App.svg")
        .replacen(r#"width="128" height="128""#, r#"width="256" height="256""#, 1);
    gdk::Texture::from_bytes(&glib::Bytes::from_owned(svg.into_bytes())).ok()
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
            style.set_color_scheme(match settings.color_scheme.as_str() {
                "light" => adw::ColorScheme::ForceLight,
                "dark" => adw::ColorScheme::ForceDark,
                _ => adw::ColorScheme::Default,
            });
            // Фирменная палитра в том варианте, который сейчас действует;
            // при смене системной темы окно перекрашивается заново.
            provider.load_from_string(&terminal::default_look(style.is_dark()).css);
        }
    }
}

/// Шрифт интерфейса: на [`UI_FONT_SCALE`] мельче системного, чтобы списки
/// и панели были плотнее. Если системный шрифт моноширинный (как бывает при
/// настройке «один шрифт на всё»), кнопки и списки набираются обычным
/// шрифтом. Терминал это не затрагивает: его шрифт задаётся отдельно.
fn tune_ui_font() {
    let Some(settings) = gtk::Settings::default() else {
        return;
    };
    let Some(name) = settings.gtk_font_name() else {
        return;
    };
    let mut description = pango::FontDescription::from_string(&name);
    if let Some(family) = description.family() {
        let context = gtk::Label::new(None).pango_context();
        let monospace = context.list_families().iter().any(|candidate| {
            candidate.name().eq_ignore_ascii_case(&family) && candidate.is_monospace()
        });
        if monospace {
            description.set_family(UI_FONT_FAMILY);
        }
    }
    let size = f64::from(description.size()) * UI_FONT_SCALE;
    if size > 0.0 {
        if description.is_size_absolute() {
            description.set_absolute_size(size);
        } else {
            description.set_size(size.round() as i32);
        }
    }
    settings.set_gtk_font_name(Some(&description.to_string()));
}

struct AppWindow {
    window: adw::ApplicationWindow,
    tab_view: adw::TabView,
    tab_bar: adw::TabBar,
    stack: gtk::Stack,
    notifier: Notifier,
    sidebar: HostsSidebar,
    /// Список подключений и область вкладок.
    split: adw::OverlaySplitView,
    /// Кнопка панели файлов в шапке.
    files_toggle: gtk::ToggleButton,
    /// Обёртка кнопки: та выезжает, пока открыта хотя бы одна вкладка.
    files_reveal: gtk::Revealer,
    tabs: RefCell<Vec<Rc<SessionTab>>>,
    settings: Rc<RefCell<Settings>>,
    /// Цвета выбранной темы для всего окна.
    theme_css: gtk::CssProvider,
    /// Кнопка сохранённых команд; хранится, чтобы жили её обработчики.
    snippets: SnippetsButton,
    /// Сохранённые базы данных (список показывает боковая панель).
    databases: Databases,
    /// Открытые вкладки баз данных.
    db_tabs: RefCell<Vec<Rc<DbTab>>>,
    next_tab_id: Cell<u32>,
}

impl AppWindow {
    fn new(app: &adw::Application) -> Rc<Self> {
        // Уведомления окна всплывают над областью вкладок.
        let notifier = Notifier::new();

        // --- боковая панель с подключениями
        let databases = Databases::new(notifier.clone());
        let sidebar = HostsSidebar::new(notifier.clone(), databases.clone());

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
            .expand_tabs(false)
            .hexpand(true)
            .visible(false)
            .build();

        let empty_page = adw::StatusPage::builder()
            .css_classes(["sshdeck-empty"])
            .title(tr!("No Open Sessions"))
            .description(tr!(
                "Double-click a connection or database on the left, \
                 or type user@host and press Enter"
            ))
            .build();
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(ANIMATION_MS)
            .build();
        match app_icon() {
            Some(icon) => empty_page.set_paintable(Some(&icon)),
            None => empty_page.set_icon_name(Some("utilities-terminal-symbolic")),
        }
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
            Some(tr!("Passwordless Login (Key) for This Tab…")),
            Some("win.key-login"),
        );
        server_section.append(
            Some(tr!("File Panel Follows cd on This Server…")),
            Some("win.cwd-tracking"),
        );
        menu.append_section(None, &server_section);
        let look_section = gio::Menu::new();
        let theme_menu = gio::Menu::new();
        let standard_themes = gio::Menu::new();
        for (id, label) in SCHEMES {
            standard_themes.append(
                Some(crate::i18n::tr(label)),
                Some(&format!("win.theme::{id}")),
            );
        }
        theme_menu.append_section(None, &standard_themes);
        let color_themes = gio::Menu::new();
        for theme in &terminal::THEMES {
            color_themes.append(Some(theme.name), Some(&format!("win.theme::{}", theme.id)));
        }
        theme_menu.append_section(None, &color_themes);
        look_section.append_submenu(Some(tr!("Theme")), &theme_menu);
        let language_menu = gio::Menu::new();
        for (code, name) in crate::i18n::LANGUAGES {
            // Названия языков — каждое на своём языке; «как в системе» — на текущем.
            let label = if code == "system" { tr!("System default") } else { name };
            language_menu.append(Some(label), Some(&format!("win.language::{code}")));
        }
        look_section.append_submenu(Some(tr!("Language")), &language_menu);
        look_section.append(Some(tr!("Terminal Font…")), Some("win.font"));
        menu.append_section(None, &look_section);
        let list_section = gio::Menu::new();
        list_section.append(Some(tr!("Reload Connection List")), Some("win.reload-hosts"));
        menu.append_section(None, &list_section);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text(tr!("Menu"))
            .menu_model(&menu)
            .primary(true)
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        // По центру шапки, а не на всю её высоту: иначе нажатая кнопка
        // подсвечивается высоким прямоугольником от края до края.
        let sidebar_toggle = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text(tr!("Show or hide the connection list"))
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        let files_toggle = gtk::ToggleButton::builder()
            .icon_name("folder-symbolic")
            .tooltip_text(tr!("Show or hide the file panel"))
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .active(settings.borrow().show_files)
            .build();
        let files_reveal =
            collapsing_revealer(gtk::RevealerTransitionType::SlideRight, &files_toggle);
        files_reveal.set_visible(false);
        snippets.widget().add_css_class("flat");
        snippets.widget().set_valign(gtk::Align::Center);

        // Одна полоса вместо заголовка и отдельной строки вкладок: вкладки
        // стоят прямо в шапке окна, как в браузере.
        let tab_area = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        tab_area.set_hexpand(true);
        tab_area.append(&tab_bar);
        // Левые кнопки окна обычно стоят в шапке боковой панели; здесь они
        // нужны, только когда панель скрыта.
        let start_controls = gtk::WindowControls::new(gtk::PackType::Start);
        let topbar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        topbar.add_css_class("sshdeck-topbar");
        topbar.append(&start_controls);
        topbar.append(&sidebar_toggle);
        topbar.append(&files_reveal);
        topbar.append(&tab_area);
        topbar.append(snippets.widget());
        topbar.append(&menu_button);
        topbar.append(&super::window_controls(gtk::PackType::End));

        let content = adw::ToolbarView::new();
        content.add_top_bar(&gtk::WindowHandle::builder().child(&topbar).build());
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
        {
            let sync = {
                let split = split.clone();
                let controls = start_controls.clone();
                move || controls.set_visible(!split.shows_sidebar() && !controls.is_empty())
            };
            sync();
            let on_sidebar = sync.clone();
            split.connect_show_sidebar_notify(move |_| on_sidebar());
            start_controls.connect_empty_notify(move |_| sync());
        }

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title(APP_TITLE)
            .default_width(1280)
            .default_height(760)
            .content(&split)
            .build();

        let this = Rc::new(Self {
            window,
            tab_view,
            tab_bar,
            stack,
            notifier,
            sidebar,
            split,
            files_toggle,
            files_reveal,
            tabs: RefCell::new(Vec::new()),
            settings,
            theme_css,
            snippets,
            databases,
            db_tabs: RefCell::new(Vec::new()),
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
            this.databases.set_parent(&this.window);
            this.databases.set_open_handler(move |profile, password| {
                if let Some(this) = weak.upgrade() {
                    this.open_db_tab(profile.clone(), password);
                }
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
            // «Как в системе»: фирменная палитра следует за светлой или
            // тёмной темой системы.
            let weak = Rc::downgrade(&this);
            adw::StyleManager::default().connect_dark_notify(move |_| {
                if let Some(this) = weak.upgrade() {
                    apply_look(&this.theme_css, &this.settings.borrow());
                }
            });
        }
        {
            // Панель файлов одна на все вкладки, её состояние сохраняется.
            let weak = Rc::downgrade(&this);
            this.files_toggle.connect_toggled(move |button| {
                let Some(this) = weak.upgrade() else { return };
                let visible = button.is_active();
                for tab in this.tabs.borrow().iter() {
                    tab.set_files_visible(visible);
                }
                this.settings.borrow_mut().show_files = visible;
                this.save_settings();
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
                        .info(tr!("Open a tab and log in to a server first")),
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
            // Язык интерфейса: сохраняется сразу, применяется при следующем
            // запуске (строки читаются из каталога один раз).
            let weak = Rc::downgrade(&this);
            let current = this.settings.borrow().language.clone();
            let action = gio::SimpleAction::new_stateful(
                "language",
                Some(glib::VariantTy::STRING),
                &current.to_variant(),
            );
            action.connect_activate(move |action, parameter| {
                let language = parameter.and_then(|value| value.get::<String>());
                let (Some(this), Some(language)) = (weak.upgrade(), language) else {
                    return;
                };
                if this.settings.borrow().language == language {
                    return;
                }
                action.set_state(&language.to_variant());
                this.settings.borrow_mut().language = language;
                this.save_settings();
                this.notifier
                    .info(tr!("The new language will be used after sshdeck is restarted."));
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
                    let db_position = this
                        .db_tabs
                        .borrow()
                        .iter()
                        .position(|tab| tab.widget().upcast_ref::<gtk::Widget>() == &child);
                    if let Some(position) = db_position {
                        let tab = this.db_tabs.borrow_mut().remove(position);
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
                    None => this.notifier.info(tr!(
                        "Open a tab and log in to a server first — the key is installed through it"
                    )),
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
                for tab in this_owned.db_tabs.borrow_mut().drain(..) {
                    tab.shutdown();
                }
                glib::Propagation::Proceed
            });
        }

        this
    }

    /// Пустой экран или вкладки; в названии окна (его видно в панели задач)
    /// — текущая сессия.
    fn sync_tabs_state(&self) {
        let has_tabs = self.tab_view.n_pages() > 0;
        self.tab_bar.set_visible(has_tabs);
        // Панель файлов есть только у вкладок с терминалом.
        let has_sessions = !self.tabs.borrow().is_empty();
        reveal(&self.files_reveal, &self.files_reveal, has_sessions, true);
        if !has_tabs {
            // Сессий не осталось — снова нужен список подключений.
            self.split.set_show_sidebar(true);
        }
        self.stack
            .set_visible_child_name(if has_tabs { "tabs" } else { "empty" });
        let title = match self.tab_view.selected_page() {
            Some(page) => format!("{} — {APP_TITLE}", page.title()),
            None => APP_TITLE.to_string(),
        };
        self.window.set_title(Some(&title));
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
                    tr!("Could Not Prepare the Socket Directory"),
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
        let (font, show_files) = {
            let settings = self.settings.borrow();
            (settings.terminal_font.clone(), settings.show_files)
        };
        let tab = SessionTab::new(
            &target,
            tab_id,
            &runtime_dir,
            &font,
            show_files,
            startup,
            on_finished,
        );
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
        // Сессия открыта — список подключений уступает место терминалу;
        // вернуть его можно кнопкой в шапке.
        self.split.set_show_sidebar(false);
        tab.focus_terminal();
        true
    }

    fn open_db_tab(self: &Rc<Self>, profile: DbProfile, password: String) {
        // Туннель к базе может пойти через соединение уже открытой вкладки
        // с тем же сервером.
        let control: ControlLookup = {
            let weak = Rc::downgrade(self);
            Rc::new(move |target: &str| {
                let this = weak.upgrade()?;
                let tabs = this.tabs.borrow();
                tabs.iter()
                    .filter(|tab| tab.target() == target)
                    .map(|tab| tab.control_path().to_path_buf())
                    .find(|path| path.exists())
            })
        };
        let title = profile.name.clone();
        let tooltip = format!("{} — {}", profile.kind_label(), describe(&profile));
        let tab = DbTab::new(profile, password, control, self.databases.clone());
        self.db_tabs.borrow_mut().push(Rc::clone(&tab));
        let page = self.tab_view.append(tab.widget());
        page.set_title(&title);
        page.set_tooltip(&glib::markup_escape_text(&tooltip));
        page.set_icon(Some(&gio::ThemedIcon::new("drive-multidisk-symbolic")));
        self.tab_view.set_selected_page(&page);
        self.sync_tabs_state();
        self.split.set_show_sidebar(false);
        tab.focus_editor();
    }

    fn save_settings(&self) {
        if let Err(err) = self.settings.borrow().save() {
            self.notifier
                .error(&tr!("Could not save settings: {}", err));
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
            .title(tr!("Terminal Font"))
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
                        .error(&tr!("Could not save settings: {}", err));
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::CSS;
    use crate::ui::hosts::AVATAR_COLORS;

    #[test]
    fn every_avatar_color_has_a_css_class() {
        for index in 0..AVATAR_COLORS {
            assert!(
                CSS.contains(&format!(".sshdeck-avatar-{index} ")),
                "{index}"
            );
        }
        assert!(!CSS.contains(&format!(".sshdeck-avatar-{AVATAR_COLORS} ")));
    }
}
