//! Интерфейс на GTK 4 и libadwaita. Весь код здесь выполняется в главном
//! потоке и не делает сетевых вызовов: с SFTP он общается через каналы
//! (`crate::sftp`).

mod databases;
mod dbtab;
mod dialogs;
mod dnd;
mod extensions;
mod files;
mod hosts;
mod snippets;
mod statusbar;
mod tab;
mod terminal;
pub mod window;

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// Сколько секунд висит обычное уведомление.
const INFO_TIMEOUT_SECS: u32 = 4;
/// Уведомление длиннее этого обрезается и получает кнопку «Подробнее».
const TOAST_MAX_CHARS: usize = 90;

/// Всплывающие уведомления поверх своей области окна.
///
/// Обычные сообщения гаснут сами, ошибки остаются до закрытия. Копии
/// разделяют один и тот же виджет.
#[derive(Clone)]
pub struct Notifier {
    overlay: adw::ToastOverlay,
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            overlay: adw::ToastOverlay::new(),
        }
    }

    /// Виджет-обёртка: поверх его содержимого показываются уведомления.
    pub fn widget(&self) -> &adw::ToastOverlay {
        &self.overlay
    }

    pub fn set_child(&self, child: &impl IsA<gtk::Widget>) {
        self.overlay.set_child(Some(child));
    }

    pub fn info(&self, text: &str) {
        self.show(text, INFO_TIMEOUT_SECS, adw::ToastPriority::Normal);
    }

    pub fn error(&self, text: &str) {
        // timeout = 0: ошибка не исчезает, пока её не закроют.
        self.show(text, 0, adw::ToastPriority::High);
    }

    fn show(&self, text: &str, timeout: u32, priority: adw::ToastPriority) {
        let (title, truncated) = shorten(text);
        let toast = adw::Toast::builder()
            .title(title)
            .use_markup(false)
            .timeout(timeout)
            .priority(priority)
            .build();
        if truncated {
            toast.set_button_label(Some(tr!("Details")));
            let overlay = self.overlay.downgrade();
            let full = text.to_string();
            toast.connect_button_clicked(move |_| {
                if let Some(overlay) = overlay.upgrade() {
                    dialogs::message(&overlay, tr!("Message"), &full);
                }
            });
        }
        self.overlay.add_toast(toast);
    }
}

/// Одна строка не длиннее [`TOAST_MAX_CHARS`]; второй элемент — была ли
/// она обрезана.
fn shorten(text: &str) -> (String, bool) {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= TOAST_MAX_CHARS {
        let changed = single_line != text.trim();
        return (single_line, changed && text.contains('\n'));
    }
    let mut short: String = single_line.chars().take(TOAST_MAX_CHARS - 1).collect();
    short.push('…');
    (short, true)
}

/// Размер в человекочитаемом виде: `512 B`, `1.4 MB` (`1,4 МБ`).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = [n_!("B"), n_!("KB"), n_!("MB"), n_!("GB"), n_!("TB")];
    if bytes < 1024 {
        return format!("{bytes} {}", crate::i18n::tr(UNITS[0]));
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let number = format!("{value:.1}").replace('.', &crate::i18n::decimal_separator().to_string());
    format!("{number} {}", crate::i18n::tr(UNITS[unit]))
}

/// Время изменения в местном часовом поясе: для этого года — день, месяц
/// и время (`10 ноя 18:09`), для прошлых лет — дата (`05.12.2025`).
pub fn format_mtime(unix_secs: u64) -> String {
    let Some(time) = i64::try_from(unix_secs)
        .ok()
        .and_then(|secs| glib::DateTime::from_unix_local(secs).ok())
    else {
        return String::new();
    };
    let this_year = glib::DateTime::now_local().is_ok_and(|now| now.year() == time.year());
    let pattern = if this_year {
        "%-d %b %H:%M"
    } else {
        "%d.%m.%Y"
    };
    time.format(pattern)
        .map(|text| text.to_string())
        .unwrap_or_default()
}

/// Кнопки окна (свернуть, развернуть, закрыть) для своей шапки. Какие кнопки
/// и с какой стороны, решает система; пустой набор не занимает места.
pub fn window_controls(side: gtk::PackType) -> gtk::WindowControls {
    let controls = gtk::WindowControls::new(side);
    controls
        .bind_property("empty", &controls, "visible")
        .invert_boolean()
        .sync_create()
        .build();
    controls
}

/// Длительность коротких переходов интерфейса, мс.
pub const ANIMATION_MS: u32 = 200;

/// `Revealer` для [`reveal`]: плавно разворачивает и сворачивает `child`.
pub fn collapsing_revealer(
    transition: gtk::RevealerTransitionType,
    child: &impl IsA<gtk::Widget>,
) -> gtk::Revealer {
    gtk::Revealer::builder()
        .transition_type(transition)
        .transition_duration(ANIMATION_MS)
        .child(child)
        .build()
}

/// Плавно показывает (`shown`) или прячет `revealer`. `outer` — виджет,
/// который после сворачивания скрывается целиком (сам `revealer` или
/// строка списка, в которой он лежит), чтобы пустое место не занимало
/// отступов. Без `animate` состояние меняется сразу.
pub fn reveal(
    revealer: &gtk::Revealer,
    outer: &impl IsA<gtk::Widget>,
    shown: bool,
    animate: bool,
) {
    let outer = outer.as_ref();
    revealer.set_transition_duration(if animate { ANIMATION_MS } else { 0 });
    if shown {
        outer.set_visible(true);
        revealer.set_reveal_child(true);
        return;
    }
    if !animate || !outer.is_visible() {
        revealer.set_reveal_child(false);
        outer.set_visible(false);
        return;
    }
    revealer.set_reveal_child(false);
    let weak = outer.downgrade();
    let handler: Rc<Cell<Option<glib::SignalHandlerId>>> = Rc::default();
    let slot = Rc::clone(&handler);
    let id = revealer.connect_child_revealed_notify(move |revealer| {
        if revealer.is_child_revealed() {
            return;
        }
        // Пока шла анимация, виджет могли снова показать.
        if !revealer.reveals_child()
            && let Some(outer) = weak.upgrade()
        {
            outer.set_visible(false);
        }
        if let Some(id) = slot.take() {
            revealer.disconnect(id);
        }
    });
    handler.set(Some(id));
}

/// Окно, в котором находится виджет (для системных диалогов выбора файлов).
pub fn parent_window(widget: &impl IsA<gtk::Widget>) -> Option<gtk::Window> {
    widget.root().and_downcast::<gtk::Window>()
}

#[cfg(test)]
mod tests {
    use super::{TOAST_MAX_CHARS, format_size, shorten};

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn short_messages_are_kept() {
        assert_eq!(shorten("Готово"), ("Готово".to_string(), false));
    }

    #[test]
    fn long_messages_are_truncated() {
        let long = "я".repeat(TOAST_MAX_CHARS + 10);
        let (short, truncated) = shorten(&long);
        assert!(truncated);
        assert_eq!(short.chars().count(), TOAST_MAX_CHARS);
        assert!(short.ends_with('…'));
    }

    #[test]
    fn multiline_messages_become_one_line_with_details() {
        let (short, truncated) = shorten("первая строка\nвторая строка");
        assert_eq!(short, "первая строка вторая строка");
        assert!(truncated);
    }
}
