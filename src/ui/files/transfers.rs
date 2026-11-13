//! Передачи панели файлов: скачивание и загрузка с карточками прогресса,
//! перетаскивание файлов наружу.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use adw::prelude::*;
use gtk::{gio, glib, pango};

use super::super::dnd::RemoteFilesProvider;
use super::super::{dialogs, format_size, reveal};
use super::tree::Side;
use super::{Inner, conflicting};
use crate::session;
use crate::sftp::{self, Command, EntryKind, FileEntry, TransferId, TransferOutcome};

static NEXT_TRANSFER: AtomicU64 = AtomicU64::new(1);

pub(super) type DoneCallback = Box<dyn FnOnce(&TransferOutcome)>;

pub(super) struct TransferRow {
    widget: gtk::Box,
    bar: gtk::ProgressBar,
    title: String,
    /// Каталог, который нужно перечитать по завершении (и после ошибки
    /// или отмены: часть файлов могла успеть появиться).
    refresh: Option<(Side, PathBuf)>,
    on_done: Option<DoneCallback>,
}

/// «a, b, c и ещё 4» — для заголовков и подтверждений.
pub(super) fn summarize(names: &[String]) -> String {
    const SHOWN: usize = 3;
    let text = names
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        let more = names.len() - SHOWN;
        return trn!("{} and {} more", "{} and {} more", more, text, more);
    }
    text
}

/// Текст вопроса о замене: что уже есть в каталоге `dest` и, если среди
/// совпавших есть каталоги, что они сольются.
fn replace_message(dest: &Path, conflicts: &[String], merge: bool) -> String {
    let mut message = tr!(
        "{} already contains: {}",
        sftp::display(dest),
        summarize(conflicts)
    );
    if merge {
        message.push('\n');
        message.push_str(tr!(
            "Folders with the same name are merged: files with the same name are replaced, other files are kept."
        ));
    }
    message
}

fn dialog_dismissed(err: &glib::Error) -> bool {
    err.matches(gtk::DialogError::Dismissed) || err.matches(gtk::DialogError::Cancelled)
}

impl Inner {
    // --- передачи

    pub(super) fn download_selected(self: &Rc<Self>) {
        let entries = self.remote.selected_transferable();
        if entries.is_empty() {
            self.notifier
                .info(tr!("Select files or folders to download"));
            return;
        }
        let dialog = gtk::FileDialog::builder()
            .title(tr!("Download to Folder"))
            .accept_label(tr!("Download Here"))
            .modal(true)
            .build();
        let weak = Rc::downgrade(self);
        dialog.select_folder(
            self.window().as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                let Some(inner) = weak.upgrade() else { return };
                match result {
                    Ok(folder) => match folder.path() {
                        Some(dest) => inner.download_to(entries, dest),
                        None => inner
                            .notifier
                            .error(tr!("The selected folder is not local")),
                    },
                    Err(err) if dialog_dismissed(&err) => {}
                    Err(err) => inner.notifier.error(&err.to_string()),
                }
            },
        );
    }

    /// Скачивает элементы сервера в локальный каталог `dest`, спросив о
    /// замене совпадающих имён.
    pub(super) fn download_to(self: &Rc<Self>, entries: Vec<FileEntry>, dest: PathBuf) {
        let (conflicts, merge) = conflicting(
            entries
                .iter()
                .map(|entry| (entry.name.as_os_str(), entry.kind == EntryKind::Dir)),
            // Битая ссылка с тем же именем тоже занимает имя.
            |name| dest.join(name).symlink_metadata().is_ok(),
        );
        let names: Vec<String> = entries.iter().map(FileEntry::display_name).collect();
        let sources: Vec<PathBuf> = entries.into_iter().map(|e| e.path).collect();
        let message = replace_message(&dest, &conflicts, merge);
        let weak = Rc::downgrade(self);
        let start = move || {
            if let Some(inner) = weak.upgrade() {
                let refresh = Some((Side::Local, dest.clone()));
                inner.start_transfer(
                    &tr!("Downloading {}", summarize(&names)),
                    |id| Command::Download {
                        id,
                        sources,
                        dest_dir: dest,
                    },
                    refresh,
                    None,
                );
            }
        };
        if conflicts.is_empty() {
            start();
        } else {
            dialogs::confirm(
                &self.root,
                tr!("Replace Existing Local Files?"),
                &message,
                tr!("Replace"),
                true,
                start,
            );
        }
    }

    pub(super) fn choose_upload(self: &Rc<Self>, folder: bool) {
        let weak = Rc::downgrade(self);
        let window = self.window();
        if folder {
            let dialog = gtk::FileDialog::builder()
                .title(tr!("Upload Folder"))
                .accept_label(tr!("Upload"))
                .modal(true)
                .build();
            dialog.select_folder(window.as_ref(), gio::Cancellable::NONE, move |result| {
                let Some(inner) = weak.upgrade() else { return };
                match result {
                    Ok(file) => inner.upload_paths(file.path().into_iter().collect(), None),
                    Err(err) if dialog_dismissed(&err) => {}
                    Err(err) => inner.notifier.error(&err.to_string()),
                }
            });
        } else {
            let dialog = gtk::FileDialog::builder()
                .title(tr!("Upload Files"))
                .accept_label(tr!("Upload"))
                .modal(true)
                .build();
            dialog.open_multiple(window.as_ref(), gio::Cancellable::NONE, move |result| {
                let Some(inner) = weak.upgrade() else { return };
                match result {
                    Ok(files) => {
                        let paths = (0..files.n_items())
                            .filter_map(|i| files.item(i).and_downcast::<gio::File>())
                            .filter_map(|file| file.path())
                            .collect();
                        inner.upload_paths(paths, None);
                    }
                    Err(err) if dialog_dismissed(&err) => {}
                    Err(err) => inner.notifier.error(&err.to_string()),
                }
            });
        }
    }

    /// Загружает локальные файлы и каталоги в удалённый каталог `dest`
    /// (по умолчанию — в текущий). Если содержимое каталога ещё не
    /// известно, сначала читает его, чтобы спросить о замене.
    pub(super) fn upload_paths(self: &Rc<Self>, paths: Vec<PathBuf>, dest: Option<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let cwd = self.remote.cwd();
        if cwd.as_os_str().is_empty() {
            self.notifier.error(tr!("No SFTP connection"));
            return;
        }
        let dest = dest.unwrap_or(cwd);
        let Some(names) = self.remote.known_names(&dest) else {
            self.state
                .borrow_mut()
                .pending_uploads
                .push((dest.clone(), paths));
            self.send(Command::List { path: dest });
            return;
        };
        self.confirm_upload(paths, dest, &names);
    }

    /// Пришло содержимое каталога: запускает ждавшие его загрузки.
    pub(super) fn start_pending_uploads(self: &Rc<Self>, path: &Path, entries: &[FileEntry]) {
        let waiting: Vec<Vec<PathBuf>> = {
            let mut state = self.state.borrow_mut();
            let (ready, rest) = std::mem::take(&mut state.pending_uploads)
                .into_iter()
                .partition(|(dir, _)| dir == path);
            state.pending_uploads = rest;
            ready.into_iter().map(|(_, paths)| paths).collect()
        };
        if waiting.is_empty() {
            return;
        }
        let existing: Vec<OsString> = entries.iter().map(|e| e.name.clone()).collect();
        for paths in waiting {
            self.confirm_upload(paths, path.to_path_buf(), &existing);
        }
    }

    /// Спрашивает о замене совпадающих имён и запускает загрузку.
    fn confirm_upload(self: &Rc<Self>, paths: Vec<PathBuf>, dest: PathBuf, existing: &[OsString]) {
        let (conflicts, merge) = conflicting(
            paths
                .iter()
                .filter_map(|path| Some((path.file_name()?, path.is_dir()))),
            |name| existing.iter().any(|known| known.as_os_str() == name),
        );
        let names: Vec<String> = paths
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();

        let weak = Rc::downgrade(self);
        let target = dest.clone();
        let start = move || {
            if let Some(inner) = weak.upgrade() {
                inner.start_transfer(
                    &tr!("Uploading {}", summarize(&names)),
                    |id| Command::Upload {
                        id,
                        sources: paths,
                        dest_dir: target.clone(),
                    },
                    Some((Side::Remote, target.clone())),
                    None,
                );
            }
        };
        if conflicts.is_empty() {
            start();
        } else {
            dialogs::confirm(
                &self.root,
                tr!("Replace Existing Files on the Server?"),
                &replace_message(&dest, &conflicts, merge),
                tr!("Replace"),
                true,
                start,
            );
        }
    }

    /// Создаёт строку прогресса и отправляет команду передачи.
    pub(super) fn start_transfer(
        self: &Rc<Self>,
        title: &str,
        make_command: impl FnOnce(TransferId) -> Command,
        refresh: Option<(Side, PathBuf)>,
        on_done: Option<DoneCallback>,
    ) {
        let id = NEXT_TRANSFER.fetch_add(1, Ordering::Relaxed);

        let label = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::Middle)
            .build();
        let cancel = gtk::Button::builder()
            .icon_name("process-stop-symbolic")
            .tooltip_text(tr!("Cancel transfer"))
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.append(&label);
        header.append(&cancel);
        let bar = gtk::ProgressBar::builder()
            .show_text(true)
            .text(tr!("Preparing…"))
            .margin_end(6)
            .build();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 2);
        widget.add_css_class("card");
        widget.add_css_class("sshdeck-transfer");
        widget.append(&header);
        widget.append(&bar);
        // Карточки завершённых передач, оставленные на время сворачивания.
        if !self.transfers_revealer.reveals_child() {
            while let Some(child) = self.transfers_box.first_child() {
                self.transfers_box.remove(&child);
            }
        }
        self.transfers_box.append(&widget);
        reveal(
            &self.transfers_revealer,
            &self.transfers_revealer,
            true,
            true,
        );

        let weak = Rc::downgrade(self);
        cancel.connect_clicked(move |button| {
            if let Some(inner) = weak.upgrade() {
                button.set_sensitive(false);
                inner.send(Command::Cancel { id });
            }
        });

        self.state.borrow_mut().transfers.insert(
            id,
            TransferRow {
                widget,
                bar,
                title: title.to_string(),
                refresh,
                on_done,
            },
        );
        self.send(make_command(id));
    }

    pub(super) fn on_progress(&self, id: TransferId, done: u64, total: u64, current: &str) {
        let state = self.state.borrow();
        let Some(row) = state.transfers.get(&id) else {
            return;
        };
        let fraction = if total == 0 {
            0.0
        } else {
            done as f64 / total as f64
        };
        row.bar.set_fraction(fraction.clamp(0.0, 1.0));
        let amounts = tr!("{} of {}", format_size(done), format_size(total));
        let text = if current.is_empty() {
            amounts
        } else {
            format!("{current} — {amounts}")
        };
        row.bar.set_text(Some(&text));
    }

    pub(super) fn on_transfer_done(self: &Rc<Self>, id: TransferId, outcome: TransferOutcome) {
        let Some(row) = self.state.borrow_mut().transfers.remove(&id) else {
            return;
        };
        if self.state.borrow().transfers.is_empty() {
            // Последняя карточка уезжает вниз вместе с блоком.
            reveal(
                &self.transfers_revealer,
                &self.transfers_revealer,
                false,
                true,
            );
        } else {
            self.transfers_box.remove(&row.widget);
        }
        if let Some((side, dir)) = &row.refresh {
            self.refresh_side(*side, dir);
        }
        match &outcome {
            TransferOutcome::Completed {
                skipped,
                size_mismatch,
                ..
            } => {
                if !size_mismatch.is_empty() {
                    let names: Vec<String> = size_mismatch
                        .iter()
                        .map(|path| sftp::display(Path::new(sftp::base_name(path))))
                        .collect();
                    self.notifier.error(&tr!(
                        "{} — the size differs from what the server reported (the file may have changed during the download): {}",
                        row.title,
                        summarize(&names)
                    ));
                }
                if !skipped.is_empty() {
                    let names: Vec<String> = skipped
                        .iter()
                        .map(|path| sftp::display(Path::new(sftp::base_name(path))))
                        .collect();
                    self.notifier.info(&tr!(
                        "{} — skipped (special files, broken links and links that loop or repeat folders): {}",
                        row.title,
                        summarize(&names)
                    ));
                }
            }
            // Конфликт бывает только при сохранении из редактора; его
            // разбирает обработчик завершения.
            TransferOutcome::Cancelled | TransferOutcome::Conflict { .. } => {}
            TransferOutcome::Failed(error) => {
                self.notifier.error(&format!("{}: {error}", row.title));
            }
        }
        if let Some(on_done) = row.on_done {
            on_done(&outcome);
        }
    }

    // --- временные файлы

    pub(super) fn new_scratch_dir(&self, kind: &str) -> Result<PathBuf, String> {
        let seq = {
            let mut state = self.state.borrow_mut();
            state.scratch_seq += 1;
            state.scratch_seq
        };
        let dir = self.scratch_dir.join(format!("{kind}-{seq}"));
        session::ensure_private_dir(&self.scratch_dir)
            .and_then(|()| session::ensure_private_dir(&dir))
            .map_err(|err| tr!("Could not create a temporary folder: {}", err))?;
        Ok(dir)
    }

    // --- перетаскивание наружу

    /// Файлы сервера для приложений снаружи: скачиваются во временный
    /// каталог, когда приёмник запросит их.
    pub(super) fn remote_files_provider(
        self: &Rc<Self>,
        entries: Vec<FileEntry>,
    ) -> RemoteFilesProvider {
        let weak = Rc::downgrade(self);
        RemoteFilesProvider::new(Box::new(move || {
            let weak = weak.clone();
            let entries = entries.clone();
            Box::pin(async move {
                let inner = weak
                    .upgrade()
                    .ok_or_else(|| tr!("the tab is closed").to_string())?;
                let dir = inner.new_scratch_dir("drag")?;
                let names: Vec<String> = entries.iter().map(FileEntry::display_name).collect();
                let raw_names: Vec<OsString> = entries.iter().map(|e| e.name.clone()).collect();
                let sources: Vec<PathBuf> = entries.into_iter().map(|e| e.path).collect();
                let (tx, rx) = async_channel::bounded(1);
                inner.start_transfer(
                    &tr!("Dragging {}", summarize(&names)),
                    |id| Command::Download {
                        id,
                        sources,
                        dest_dir: dir.clone(),
                    },
                    None,
                    Some(Box::new(move |outcome| {
                        let _ = tx.try_send(outcome.clone());
                    })),
                );
                drop(inner);
                match rx.recv().await {
                    Ok(TransferOutcome::Completed { .. }) => {
                        Ok(raw_names.iter().map(|name| dir.join(name)).collect())
                    }
                    Ok(TransferOutcome::Cancelled | TransferOutcome::Conflict { .. }) => {
                        Err(tr!("the transfer was cancelled").to_string())
                    }
                    Ok(TransferOutcome::Failed(error)) => Err(error),
                    Err(_) => Err(tr!("the transfer was interrupted").to_string()),
                }
            })
        }))
    }
}
