//! Интерфейс на GTK 4 и libadwaita. Весь код здесь выполняется в главном
//! потоке и не делает сетевых вызовов: с SFTP он общается через каналы
//! (`crate::sftp`).

mod dialogs;
mod dnd;
mod files;
mod hosts;
mod snippets;
mod statusbar;
mod tab;
mod terminal;
pub mod window;

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
            toast.set_button_label(Some("Подробнее"));
            let overlay = self.overlay.downgrade();
            let full = text.to_string();
            toast.connect_button_clicked(move |_| {
                if let Some(overlay) = overlay.upgrade() {
                    dialogs::message(&overlay, "Сообщение", &full);
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

/// Размер в человекочитаемом виде: `512 Б`, `1,4 МБ`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    if bytes < 1024 {
        return format!("{bytes} Б");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit]).replace('.', ",")
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

/// Окно, в котором находится виджет (для системных диалогов выбора файлов).
pub fn parent_window(widget: &impl IsA<gtk::Widget>) -> Option<gtk::Window> {
    widget.root().and_downcast::<gtk::Window>()
}

#[cfg(test)]
mod tests {
    use super::{TOAST_MAX_CHARS, format_size, shorten};

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 Б");
        assert_eq!(format_size(1023), "1023 Б");
        assert_eq!(format_size(1024), "1,0 КБ");
        assert_eq!(format_size(1536), "1,5 КБ");
        assert_eq!(format_size(5 * 1024 * 1024), "5,0 МБ");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3,0 ГБ");
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
