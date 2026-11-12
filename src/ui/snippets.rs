//! Сохранённые команды: кнопка в заголовке окна со списком команд, которые
//! одним щелчком отправляются в терминал текущей вкладки.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, pango};

use super::{Notifier, dialogs};
use crate::settings::{Settings, Snippet};

const CANCEL: &str = "cancel";
const ACCEPT: &str = "accept";

/// Выполняет команду в текущей вкладке; `false`, если вкладки нет.
type RunHandler = Box<dyn Fn(&str) -> bool>;

pub struct SnippetsButton {
    inner: Rc<Inner>,
}

struct Inner {
    button: gtk::MenuButton,
    popover: gtk::Popover,
    list: gtk::Box,
    settings: Rc<RefCell<Settings>>,
    notifier: Notifier,
    run: RefCell<Option<RunHandler>>,
}

impl SnippetsButton {
    pub fn new(settings: Rc<RefCell<Settings>>, notifier: Notifier) -> Self {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let add_button = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("list-add-symbolic")
                    .label(tr!("Add Command…"))
                    .build(),
            )
            .css_classes(["flat"])
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_width_request(300);
        content.append(&list);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&add_button);
        let popover = gtk::Popover::builder().child(&content).build();
        let button = gtk::MenuButton::builder()
            .icon_name("utilities-terminal-symbolic")
            .tooltip_text(tr!("Saved commands"))
            .popover(&popover)
            .build();

        let inner = Rc::new(Inner {
            button,
            popover,
            list,
            settings,
            notifier,
            run: RefCell::new(None),
        });
        {
            let weak = Rc::downgrade(&inner);
            add_button.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.edit(None);
                }
            });
        }
        inner.rebuild();
        Self { inner }
    }

    pub fn widget(&self) -> &gtk::MenuButton {
        &self.inner.button
    }

    pub fn set_run_handler(&self, handler: impl Fn(&str) -> bool + 'static) {
        *self.inner.run.borrow_mut() = Some(Box::new(handler));
    }
}

impl Inner {
    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let snippets = self.settings.borrow().snippets.clone();
        if snippets.is_empty() {
            self.list.append(
                &gtk::Label::builder()
                    .label(tr!(
                        "Your commands will appear here. Click a command to run it \
                         in the terminal of the current tab."
                    ))
                    .wrap(true)
                    .xalign(0.0)
                    .max_width_chars(36)
                    .margin_top(6)
                    .margin_bottom(6)
                    .margin_start(6)
                    .margin_end(6)
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
        for (index, snippet) in snippets.iter().enumerate() {
            let labels = gtk::Box::new(gtk::Orientation::Vertical, 0);
            labels.append(
                &gtk::Label::builder()
                    .label(&snippet.name)
                    .xalign(0.0)
                    .ellipsize(pango::EllipsizeMode::End)
                    .build(),
            );
            if snippet.command != snippet.name {
                labels.append(
                    &gtk::Label::builder()
                        .label(&snippet.command)
                        .xalign(0.0)
                        .ellipsize(pango::EllipsizeMode::End)
                        .css_classes(["caption", "dim-label", "monospace"])
                        .build(),
                );
            }
            let run_button = gtk::Button::builder()
                .child(&labels)
                .hexpand(true)
                .tooltip_text(tr!("Run in the terminal:\n{}", snippet.command))
                .css_classes(["flat"])
                .build();
            let edit_button = gtk::Button::builder()
                .icon_name("document-edit-symbolic")
                .tooltip_text(tr!("Edit"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let delete_button = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text(tr!("Delete"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();

            let weak = Rc::downgrade(self);
            let command = snippet.command.clone();
            run_button.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.run(&command);
                }
            });
            let weak = Rc::downgrade(self);
            edit_button.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.edit(Some(index));
                }
            });
            let weak = Rc::downgrade(self);
            delete_button.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.delete(index);
                }
            });

            let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            row.append(&run_button);
            row.append(&edit_button);
            row.append(&delete_button);
            self.list.append(&row);
        }
    }

    fn run(&self, command: &str) {
        self.popover.popdown();
        let done = self.run.borrow().as_ref().is_some_and(|run| run(command));
        if !done {
            self.notifier
                .info(tr!("No open tab to send the command to"));
        }
    }

    fn save(self: &Rc<Self>) {
        if let Err(err) = self.settings.borrow().save() {
            self.notifier
                .error(&tr!("Could not save settings: {}", err));
        }
        self.rebuild();
    }

    fn delete(self: &Rc<Self>, index: usize) {
        let Some(snippet) = self.settings.borrow().snippets.get(index).cloned() else {
            return;
        };
        self.popover.popdown();
        let weak = Rc::downgrade(self);
        dialogs::confirm(
            &self.button,
            tr!("Delete Command?"),
            &tr!("“{}” will be removed from the saved commands.", snippet.name),
            tr!("Delete"),
            true,
            move || {
                let Some(inner) = weak.upgrade() else { return };
                {
                    let mut settings = inner.settings.borrow_mut();
                    // Список мог измениться, пока был открыт диалог.
                    if let Some(position) = settings.snippets.iter().position(|s| s == &snippet) {
                        settings.snippets.remove(position);
                    }
                }
                inner.save();
            },
        );
    }

    /// Диалог новой (`None`) или существующей команды.
    fn edit(self: &Rc<Self>, index: Option<usize>) {
        let original = index.and_then(|index| self.settings.borrow().snippets.get(index).cloned());
        self.popover.popdown();

        let name_row = adw::EntryRow::builder()
            .title(trc!("connection or command", "Name"))
            .text(original.as_ref().map_or("", |s| s.name.as_str()))
            .activates_default(true)
            .build();
        let command_row = adw::EntryRow::builder()
            .title(tr!("Command"))
            .text(original.as_ref().map_or("", |s| s.command.as_str()))
            .activates_default(true)
            .build();
        command_row.add_css_class("monospace");
        let fields = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .width_request(420)
            .build();
        fields.append(&name_row);
        fields.append(&command_row);

        let dialog = adw::AlertDialog::new(
            Some(if original.is_some() {
                tr!("Edit Command")
            } else {
                tr!("New Command")
            }),
            Some(tr!(
                "Clicking a command in the list types it into the terminal of the current tab \
                 and presses Enter."
            )),
        );
        dialog.add_responses(&[(CANCEL, tr!("Cancel")), (ACCEPT, tr!("Save"))]);
        dialog.set_close_response(CANCEL);
        dialog.set_default_response(Some(ACCEPT));
        dialog.set_response_appearance(ACCEPT, adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled(ACCEPT, original.is_some());
        dialog.set_extra_child(Some(&fields));
        dialog.set_focus(Some(&name_row));

        let weak_dialog = dialog.downgrade();
        command_row.connect_changed(move |row| {
            if let Some(dialog) = weak_dialog.upgrade() {
                dialog.set_response_enabled(ACCEPT, !row.text().trim().is_empty());
            }
        });

        let weak = Rc::downgrade(self);
        dialog.choose(Some(&self.button), gio::Cancellable::NONE, move |response| {
            if response != ACCEPT {
                return;
            }
            let Some(inner) = weak.upgrade() else { return };
            let command = single_line(&command_row.text());
            if command.is_empty() {
                return;
            }
            let name = match single_line(&name_row.text()) {
                name if name.is_empty() => command.clone(),
                name => name,
            };
            let snippet = Snippet { name, command };
            {
                let mut settings = inner.settings.borrow_mut();
                let position = original
                    .as_ref()
                    .and_then(|original| settings.snippets.iter().position(|s| s == original));
                match position {
                    Some(position) => settings.snippets[position] = snippet,
                    None => settings.snippets.push(snippet),
                }
            }
            inner.save();
        });
    }
}

/// Одна строка без управляющих символов и пробелов по краям.
fn single_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::single_line;

    #[test]
    fn commands_are_single_line() {
        assert_eq!(single_line("  df -h\n"), "df -h");
        assert_eq!(single_line("a\tb\nc"), "a b c");
        assert_eq!(single_line(" \n "), "");
    }
}
