//! Терминал вкладки: виджет VTE, в котором работает системный `ssh`.
//!
//! Содержимое терминала (включая вводимые пароли) приложение не читает и
//! никуда не записывает — ввод идёт напрямую в pty дочернего процесса.

use std::cell::RefCell;

use gtk::{gdk, gio, glib, pango};
use vte::prelude::*;

const SCROLLBACK_LINES: i64 = 10_000;

const ZOOM_STEP: f64 = 1.1;
const ZOOM_MIN: f64 = 0.5;
const ZOOM_MAX: f64 = 3.0;

/// Флаги PCRE2 для регулярных выражений VTE: MULTILINE | UTF | UCP |
/// NO_UTF_CHECK.
const REGEX_FLAGS: u32 = 0x0000_0400 | 0x0008_0000 | 0x0002_0000 | 0x4000_0000;
const URL_PATTERN: &str = r#"https?://[^\s<>"'`]+[^\s<>"'`.,;:!?)\]]"#;
const IP_PATTERN: &str =
    r"\b(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)\b(?::\d{1,5})?";

/// Цветовая тема терминала: фон, текст и 16 цветов ANSI.
pub struct Theme {
    pub id: &'static str,
    pub name: &'static str,
    foreground: &'static str,
    background: &'static str,
    palette: [&'static str; 16],
}

/// Собственная тёмная палитра sshdeck: стандартные «Как в системе» и
/// «Тёмная» берут её, а не серые цвета Adwaita.
const DECK_NIGHT: Theme = Theme {
    id: "system",
    name: n_!("Match window style"),
    foreground: "#d6dae3",
    background: "#15171d",
    palette: [
        "#20232b", "#ff6b7d", "#4ade80", "#f5c45e", "#6d8dff", "#c38bff", "#3fd2e6", "#c5cad6",
        "#4d5363", "#ff8a98", "#7ff0a6", "#ffd98a", "#94acff", "#d6adff", "#78e3f2", "#f2f4f8",
    ],
};
/// Светлая пара к [`DECK_NIGHT`].
const DECK_DAY: Theme = Theme {
    id: "system",
    name: n_!("Match window style"),
    foreground: "#1f2430",
    background: "#fbfbfd",
    palette: [
        "#1f2430", "#d32f45", "#1a8f4a", "#a26a00", "#3e63e6", "#8a4fd8", "#0b7f8f", "#6b7280",
        "#4b5263", "#e5484d", "#2bb062", "#c58a00", "#5b7cff", "#a774ef", "#1599ab", "#9aa1ad",
    ],
};

const SOLARIZED_PALETTE: [&str; 16] = [
    "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
    "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
];

/// Темы с собственными цветами; тема `system` в список не входит.
pub const THEMES: [Theme; 8] = [
    Theme {
        id: "dracula",
        name: "Dracula",
        foreground: "#f8f8f2",
        background: "#282a36",
        palette: [
            "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd",
            "#f8f8f2", "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df",
            "#a4ffff", "#ffffff",
        ],
    },
    Theme {
        id: "nord",
        name: "Nord",
        foreground: "#d8dee9",
        background: "#2e3440",
        palette: [
            "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0",
            "#e5e9f0", "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead",
            "#8fbcbb", "#eceff4",
        ],
    },
    Theme {
        id: "gruvbox-dark",
        name: "Gruvbox Dark",
        foreground: "#ebdbb2",
        background: "#282828",
        palette: [
            "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a",
            "#a89984", "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b",
            "#8ec07c", "#ebdbb2",
        ],
    },
    Theme {
        id: "tokyo-night",
        name: "Tokyo Night",
        foreground: "#c0caf5",
        background: "#1a1b26",
        palette: [
            "#15161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff",
            "#a9b1d6", "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7",
            "#7dcfff", "#c0caf5",
        ],
    },
    Theme {
        id: "one-dark",
        name: "One Dark",
        foreground: "#abb2bf",
        background: "#282c34",
        palette: [
            "#282c34", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2",
            "#abb2bf", "#5c6370", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd",
            "#56b6c2", "#ffffff",
        ],
    },
    Theme {
        id: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        foreground: "#cdd6f4",
        background: "#1e1e2e",
        palette: [
            "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5",
            "#bac2de", "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7",
            "#94e2d5", "#a6adc8",
        ],
    },
    Theme {
        id: "solarized-dark",
        name: "Solarized Dark",
        foreground: "#839496",
        background: "#002b36",
        palette: SOLARIZED_PALETTE,
    },
    Theme {
        id: "solarized-light",
        name: "Solarized Light",
        foreground: "#657b83",
        background: "#fdf6e3",
        palette: SOLARIZED_PALETTE,
    },
];

thread_local! {
    /// Выбранная тема; одна на все терминалы приложения.
    static CURRENT_THEME: RefCell<String> = RefCell::new(DECK_NIGHT.id.to_string());
}

/// Оформление всего окна в цветах темы.
pub struct AppLook {
    /// CSS, переопределяющий цвета libadwaita.
    pub css: String,
    /// Тёмная ли тема: от этого зависят остальные цвета интерфейса.
    pub dark: bool,
}

/// Цвета интерфейса для темы с собственной палитрой; `None` для темы,
/// следующей за системой, и для неизвестного идентификатора.
pub fn app_look(id: &str) -> Option<AppLook> {
    look_for(THEMES.iter().find(|theme| theme.id == id)?)
}

/// Фирменное оформление sshdeck в светлом или тёмном варианте — для
/// стандартных тем «Как в системе», «Светлая» и «Тёмная».
pub fn default_look(dark: bool) -> AppLook {
    look_for(if dark { &DECK_NIGHT } else { &DECK_DAY })
        .expect("built-in palettes are written as #rrggbb")
}

fn look_for(theme: &Theme) -> Option<AppLook> {
    let background = Rgb::parse(theme.background)?;
    let foreground = Rgb::parse(theme.foreground)?;
    // Синий из палитры темы служит акцентом: кнопки, выделение, фокус.
    let accent = Rgb::parse(theme.palette[4])?;
    let dark = background.luminance() < 0.5;
    let black = Rgb(0, 0, 0);

    // Список файлов и терминал — основной фон темы; заголовок и боковая
    // панель чуть темнее, всплывающие поверхности чуть светлее.
    let chrome = background.mix(black, if dark { 0.16 } else { 0.05 });
    let sidebar = background.mix(black, if dark { 0.24 } else { 0.08 });
    let raised = background.mix(foreground, 0.08);
    let accent_text = if accent.luminance() > 0.45 {
        background.mix(black, if dark { 0.3 } else { 0.85 })
    } else {
        Rgb(255, 255, 255)
    };

    let colors = [
        ("window-bg-color", chrome),
        ("window-fg-color", foreground),
        ("view-bg-color", background),
        ("view-fg-color", foreground),
        ("headerbar-bg-color", chrome),
        ("headerbar-fg-color", foreground),
        ("headerbar-backdrop-color", chrome),
        ("sidebar-bg-color", sidebar),
        ("sidebar-fg-color", foreground),
        ("sidebar-backdrop-color", sidebar),
        ("secondary-sidebar-bg-color", sidebar),
        ("secondary-sidebar-fg-color", foreground),
        ("card-bg-color", raised),
        ("card-fg-color", foreground),
        ("popover-bg-color", raised),
        ("popover-fg-color", foreground),
        ("dialog-bg-color", raised),
        ("dialog-fg-color", foreground),
        ("accent-bg-color", accent),
        ("accent-fg-color", accent_text),
        ("accent-color", accent),
    ];
    // libadwaita 1.6+ читает переменные CSS, более ранние правила —
    // именованные цвета; задаём оба вида.
    let mut css = String::new();
    for (name, color) in colors {
        css.push_str(&format!(
            "@define-color {} {};\n",
            name.replace('-', "_"),
            color.hex()
        ));
    }
    css.push_str(":root {\n");
    for (name, color) in colors {
        css.push_str(&format!("  --{name}: {};\n", color.hex()));
    }
    css.push_str("}\n");
    Some(AppLook { css, dark })
}

/// Цвет без прозрачности.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rgb(u8, u8, u8);

impl Rgb {
    /// Разбирает запись `#rrggbb`.
    fn parse(text: &str) -> Option<Self> {
        let hex = text.strip_prefix('#').filter(|hex| hex.len() == 6)?;
        let channel = |index: usize| u8::from_str_radix(hex.get(index..index + 2)?, 16).ok();
        Some(Self(channel(0)?, channel(2)?, channel(4)?))
    }

    fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    /// Смесь с другим цветом: `amount` = 0 — этот цвет, 1 — другой.
    fn mix(self, other: Self, amount: f64) -> Self {
        let blend = |a: u8, b: u8| {
            (f64::from(a) + (f64::from(b) - f64::from(a)) * amount)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        Self(
            blend(self.0, other.0),
            blend(self.1, other.1),
            blend(self.2, other.2),
        )
    }

    /// Воспринимаемая яркость от 0 до 1.
    fn luminance(self) -> f64 {
        (0.299 * f64::from(self.0) + 0.587 * f64::from(self.1) + 0.114 * f64::from(self.2)) / 255.0
    }
}

/// Запоминает тему для всех терминалов. Уже открытые терминалы
/// перекрашиваются вызовом [`refresh_colors`].
pub fn set_theme(id: &str) {
    CURRENT_THEME.with(|current| *current.borrow_mut() = id.to_string());
}

fn current_theme(dark: bool) -> &'static Theme {
    let id = CURRENT_THEME.with(|current| current.borrow().clone());
    THEMES
        .iter()
        .find(|theme| theme.id == id)
        .unwrap_or(if dark { &DECK_NIGHT } else { &DECK_DAY })
}

pub struct TerminalView {
    pub term: vte::Terminal,
    /// Терминал с полосой прокрутки — именно это вставляется в окно.
    pub widget: gtk::ScrolledWindow,
}

impl TerminalView {
    /// `on_notice` получает короткие сообщения для пользователя (например,
    /// «адрес скопирован»).
    pub fn new(font: &str, on_notice: impl Fn(&str) + 'static) -> Self {
        let term = vte::Terminal::builder()
            .hexpand(true)
            .vexpand(true)
            .scrollback_lines(SCROLLBACK_LINES as u32)
            .scroll_on_keystroke(true)
            .build();
        term.set_scrollback_lines(SCROLLBACK_LINES);
        apply_font(&term, font);
        install_shortcuts(&term);
        install_matches(&term, on_notice);
        follow_color_scheme(&term);

        // VTE сам пересчитывает строки и столбцы при изменении размера и
        // сообщает их pty, так что удалённая сторона получает SIGWINCH.
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&term)
            .hexpand(true)
            .vexpand(true)
            .build();
        Self { term, widget }
    }
}

pub fn apply_font(term: &vte::Terminal, font: &str) {
    let description = pango::FontDescription::from_string(font);
    term.set_font(Some(&description));
}

/// Красит терминал под выбранную тему и перекрашивает при смене
/// оформления окна.
fn follow_color_scheme(term: &vte::Terminal) {
    refresh_colors(term);

    let weak = term.downgrade();
    let handler = adw::StyleManager::default().connect_dark_notify(move |_| {
        if let Some(term) = weak.upgrade() {
            refresh_colors(&term);
        }
    });
    // Менеджер стилей живёт всё время работы приложения; обработчик
    // снимается вместе с терминалом.
    let handler = std::cell::Cell::new(Some(handler));
    term.connect_destroy(move |_| {
        if let Some(handler) = handler.take() {
            adw::StyleManager::default().disconnect(handler);
        }
    });
}

/// Применяет текущую тему к терминалу.
pub fn refresh_colors(term: &vte::Terminal) {
    let theme = current_theme(adw::StyleManager::default().is_dark());
    let parse = |color: &str| gdk::RGBA::parse(color).ok();
    let palette: Vec<gdk::RGBA> = theme.palette.iter().filter_map(|c| parse(c)).collect();
    let (Some(foreground), Some(background)) = (parse(theme.foreground), parse(theme.background))
    else {
        return;
    };
    if palette.len() != theme.palette.len() {
        return;
    }
    let palette: Vec<&gdk::RGBA> = palette.iter().collect();
    term.set_colors(Some(&foreground), Some(&background), &palette);
}

/// Ctrl+Shift+C / Ctrl+Shift+V и масштаб Ctrl +/−/0.
///
/// Копирование и вставка ловятся отдельным обработчиком клавиш, а не
/// триггерами GTK: при нелатинской раскладке (русская, казахская) GTK
/// получает `М` вместо `v` и, когда зажат Shift, не находит сочетание
/// даже по физической клавише — тогда VTE отправлял в pty `^V`.
fn install_shortcuts(term: &vte::Terminal) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = term.downgrade();
    keys.connect_key_pressed(move |_, keyval, keycode, state| {
        let Some(term) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let mods = state
            & (gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::SHIFT_MASK
                | gdk::ModifierType::ALT_MASK
                | gdk::ModifierType::SUPER_MASK);
        if mods != gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK {
            return glib::Propagation::Proceed;
        }
        match latin_letter(&term, keyval, keycode) {
            Some('c') => term.copy_clipboard_format(vte::Format::Text),
            Some('v') => term.paste_clipboard(),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    term.add_controller(keys);

    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    for trigger in ["<Control>plus", "<Control>equal", "<Control>KP_Add"] {
        add_shortcut(&controller, trigger, |term| zoom(term, ZOOM_STEP));
    }
    for trigger in ["<Control>minus", "<Control>KP_Subtract"] {
        add_shortcut(&controller, trigger, |term| zoom(term, 1.0 / ZOOM_STEP));
    }
    add_shortcut(&controller, "<Control>0", |term| term.set_font_scale(1.0));
    term.add_controller(controller);
}

/// Латинская буква нажатой клавиши независимо от раскладки: сам `keyval`,
/// если он латинский, иначе буква той же физической клавиши из латинской
/// раскладки (на ЙЦУКЕН клавиша `М` даёт `v`).
fn latin_letter(widget: &impl IsA<gtk::Widget>, keyval: gdk::Key, keycode: u32) -> Option<char> {
    let latin = |key: gdk::Key| key.to_lower().to_unicode().filter(char::is_ascii_lowercase);
    latin(keyval).or_else(|| {
        widget
            .display()
            .map_keycode(keycode)?
            .into_iter()
            .filter(|(entry, _)| entry.level() == 0)
            .find_map(|(_, key)| latin(key))
    })
}

fn zoom(term: &vte::Terminal, factor: f64) {
    term.set_font_scale((term.font_scale() * factor).clamp(ZOOM_MIN, ZOOM_MAX));
}

fn add_shortcut(
    controller: &gtk::ShortcutController,
    trigger: &str,
    handler: impl Fn(&vte::Terminal) + 'static,
) {
    let action = gtk::CallbackAction::new(move |widget, _| {
        if let Some(term) = widget.downcast_ref::<vte::Terminal>() {
            handler(term);
        }
        glib::Propagation::Stop
    });
    let shortcut = gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(trigger), Some(action));
    controller.add_shortcut(shortcut);
}

/// Подсвечивает под указателем ссылки и IP-адреса. Ctrl+щелчок открывает
/// ссылку в браузере, а адрес копирует в буфер обмена.
fn install_matches(term: &vte::Terminal, on_notice: impl Fn(&str) + 'static) {
    for pattern in [URL_PATTERN, IP_PATTERN] {
        if let Ok(regex) = vte::Regex::for_match(pattern, REGEX_FLAGS) {
            let tag = term.match_add_regex(&regex, 0);
            term.match_set_cursor_name(tag, "pointer");
        }
    }

    let gesture = gtk::GestureClick::builder()
        .button(gdk::BUTTON_PRIMARY)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    let weak = term.downgrade();
    gesture.connect_pressed(move |gesture, _, x, y| {
        let Some(term) = weak.upgrade() else { return };
        if !gesture
            .current_event_state()
            .contains(gdk::ModifierType::CONTROL_MASK)
        {
            return;
        }
        let (Some(text), _) = term.check_match_at(x, y) else {
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        if text.starts_with("http://") || text.starts_with("https://") {
            let window = term.root().and_downcast::<gtk::Window>();
            gtk::UriLauncher::new(&text).launch(window.as_ref(), gio::Cancellable::NONE, |_| {});
        } else {
            term.clipboard().set_text(&text);
            on_notice(&tr!("Copied: {}", text));
        }
    });
    term.add_controller(gesture);
}

/// Запускает команду в терминале. Об ошибке запуска сообщает `on_error`;
/// о завершении процесса — сигнал `child-exited` терминала.
pub fn spawn(term: &vte::Terminal, argv: &[String], on_error: impl FnOnce(String) + 'static) {
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    term.spawn_async(
        vte::PtyFlags::DEFAULT,
        None,
        &argv,
        &[],
        glib::SpawnFlags::DEFAULT,
        || {},
        -1,
        gio::Cancellable::NONE,
        move |result| {
            if let Err(err) = result {
                on_error(err.to_string());
            }
        },
    );
}

/// Код выхода из статуса `wait(2)`; `None`, если процесс убит сигналом.
pub fn exit_code(wait_status: i32) -> Option<i32> {
    if wait_status & 0x7f == 0 {
        Some((wait_status >> 8) & 0xff)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{DECK_DAY, DECK_NIGHT, Rgb, THEMES, app_look, default_look};

    #[test]
    fn theme_ids_are_unique_and_colors_are_hex() {
        let mut ids: Vec<&str> = THEMES.iter().map(|theme| theme.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), THEMES.len());
        assert!(!ids.contains(&"system"));

        for theme in &THEMES {
            assert!(Rgb::parse(theme.foreground).is_some(), "{}", theme.id);
            assert!(Rgb::parse(theme.background).is_some(), "{}", theme.id);
            assert!(
                theme.palette.iter().all(|c| Rgb::parse(c).is_some()),
                "{}",
                theme.id
            );
        }
    }

    #[test]
    fn color_math() {
        assert_eq!(Rgb::parse("#ff8000"), Some(Rgb(255, 128, 0)));
        assert_eq!(Rgb::parse("ff8000"), None);
        assert_eq!(Rgb::parse("#ff80"), None);
        assert_eq!(Rgb(255, 128, 0).hex(), "#ff8000");
        assert_eq!(Rgb(0, 0, 0).mix(Rgb(200, 100, 50), 0.5), Rgb(100, 50, 25));
        assert!(Rgb(255, 255, 255).luminance() > 0.99);
        assert!(Rgb(0, 0, 0).luminance() < 0.01);
    }

    #[test]
    fn every_theme_has_an_app_look() {
        for theme in &THEMES {
            let look = app_look(theme.id).expect(theme.id);
            assert_eq!(look.dark, theme.id != "solarized-light", "{}", theme.id);
            // Фон списка и терминала — фон темы.
            assert!(
                look.css
                    .contains(&format!("--view-bg-color: {};", theme.background)),
                "{}",
                theme.id
            );
            assert!(look.css.contains("@define-color accent_bg_color "));
        }
        assert!(app_look("system").is_none());
        assert!(app_look("no-such-theme").is_none());
    }

    #[test]
    fn default_look_matches_requested_variant() {
        let night = default_look(true);
        let day = default_look(false);
        assert!(night.dark);
        assert!(!day.dark);
        let night_bg = format!("--view-bg-color: {};", DECK_NIGHT.background);
        let day_bg = format!("--view-bg-color: {};", DECK_DAY.background);
        assert!(night.css.contains(&night_bg));
        assert!(day.css.contains(&day_bg));
    }
}
