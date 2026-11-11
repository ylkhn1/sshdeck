//! Небольшие модальные диалоги в стиле libadwaita: подтверждение, ввод
//! строки, сообщение.

use adw::prelude::*;
use gtk::gio;

const CANCEL: &str = "cancel";
const ACCEPT: &str = "accept";

/// Спрашивает подтверждение. `on_confirm` вызывается только при согласии.
/// Для опасных действий кнопка подтверждения выделена красным, а кнопкой
/// по умолчанию остаётся «Отмена».
pub fn confirm(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    confirm_label: &str,
    destructive: bool,
    on_confirm: impl FnOnce() + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[(CANCEL, "Отмена"), (ACCEPT, confirm_label)]);
    dialog.set_close_response(CANCEL);
    if destructive {
        dialog.set_response_appearance(ACCEPT, adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some(CANCEL));
    } else {
        dialog.set_response_appearance(ACCEPT, adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some(ACCEPT));
    }
    dialog.choose(Some(parent), gio::Cancellable::NONE, move |response| {
        if response == ACCEPT {
            on_confirm();
        }
    });
}

/// Показывает сообщение с одной кнопкой «Закрыть».
pub fn message(parent: &impl IsA<gtk::Widget>, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response(CANCEL, "Закрыть");
    dialog.set_close_response(CANCEL);
    dialog.present(Some(parent));
}

/// Запрашивает строку. `on_accept` получает введённый текст (без пробелов
/// по краям) и вызывается, только если он не пуст.
pub fn prompt_text(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    initial: &str,
    accept_label: &str,
    on_accept: impl FnOnce(String) + 'static,
) {
    let entry = gtk::Entry::builder()
        .text(initial)
        .activates_default(true)
        .build();

    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[(CANCEL, "Отмена"), (ACCEPT, accept_label)]);
    dialog.set_close_response(CANCEL);
    dialog.set_default_response(Some(ACCEPT));
    dialog.set_response_appearance(ACCEPT, adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled(ACCEPT, !initial.trim().is_empty());
    dialog.set_extra_child(Some(&entry));
    dialog.set_focus(Some(&entry));

    let weak_dialog = dialog.downgrade();
    entry.connect_changed(move |entry| {
        if let Some(dialog) = weak_dialog.upgrade() {
            dialog.set_response_enabled(ACCEPT, !entry.text().trim().is_empty());
        }
    });

    dialog.choose(Some(parent), gio::Cancellable::NONE, move |response| {
        let text = entry.text().trim().to_string();
        if response == ACCEPT && !text.is_empty() {
            on_accept(text);
        }
    });
}
