//! Правка удалённых файлов в локальном редакторе: копия скачивается во
//! временный каталог, а её сохранения заливаются обратно на сервер.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use adw::prelude::*;
use gtk::{gio, glib};

use super::super::{dialogs, format_mtime, format_size};
use super::Inner;
use super::tree::Side;
use crate::sftp::{self, Command, FileEntry, RemoteStamp, SaveMode, TransferOutcome};

/// Файлы крупнее этого открываются в редакторе только после подтверждения.
const LARGE_EDIT_BYTES: u64 = 50 * 1024 * 1024;
/// Пауза после последнего изменения файла перед заливкой на сервер:
/// редакторы сохраняют в несколько приёмов.
const EDIT_DEBOUNCE: Duration = Duration::from_millis(400);

/// Время изменения и размер локального файла.
type Stamp = (Option<SystemTime>, u64);

/// Файл, открытый в локальном редакторе.
pub(super) struct EditWatch {
    /// Куда сохранять; меняется, если правку сохранили копией рядом.
    remote: RefCell<PathBuf>,
    /// Состояние файла на сервере после скачивания или последней заливки:
    /// с ним сверяемся перед сохранением.
    remote_stamp: Cell<Option<RemoteStamp>>,
    local: PathBuf,
    /// Держит наблюдение за каталогом с копией.
    pub(super) monitor: Option<gio::FileMonitor>,
    /// Состояние файла, которое уже отправлено на сервер.
    stamp: Cell<Option<Stamp>>,
    uploading: Cell<bool>,
    /// Файл изменился, пока шла предыдущая заливка.
    dirty: Cell<bool>,
    generation: Cell<u32>,
}

fn file_stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

/// Имя первой копии для подсказки в диалоге (сервер выберет свободное).
fn edit_copy_name(remote: &Path) -> OsString {
    sftp::copy_name(sftp::base_name(remote), 1)
}

impl Inner {
    // --- правка в локальном редакторе

    pub(super) fn open_for_edit(self: &Rc<Self>, entry: FileEntry) {
        // Файл уже открыт: повторное скачивание затёрло бы несохранённую
        // на сервере правку, поэтому просто открываем копию ещё раз.
        let existing = self
            .state
            .borrow()
            .edits
            .iter()
            .find(|watch| *watch.remote.borrow() == entry.path)
            .map(|watch| watch.local.clone());
        if let Some(local) = existing {
            self.open_local(&local);
            return;
        }

        if entry.size.unwrap_or(0) > LARGE_EDIT_BYTES {
            let weak = Rc::downgrade(self);
            let size = format_size(entry.size.unwrap_or(0));
            dialogs::confirm(
                &self.root,
                &tr!("Open “{}”?", entry.display_name()),
                &tr!(
                    "The file is large ({}); it will be downloaded in full to a temporary folder.",
                    size
                ),
                tr!("Open"),
                false,
                move || {
                    if let Some(inner) = weak.upgrade() {
                        inner.begin_edit(entry);
                    }
                },
            );
        } else {
            self.begin_edit(entry);
        }
    }

    fn begin_edit(self: &Rc<Self>, entry: FileEntry) {
        let dir = match self.new_scratch_dir("edit") {
            Ok(dir) => dir,
            Err(message) => {
                self.notifier.error(&message);
                return;
            }
        };
        let local = dir.join(&entry.name);
        let remote = entry.path.clone();
        let weak = Rc::downgrade(self);
        self.start_transfer(
            &tr!("Opening {}", entry.display_name()),
            |id| Command::Download {
                id,
                sources: vec![entry.path],
                dest_dir: dir,
            },
            None,
            Some(Box::new(move |outcome| {
                if let (Some(inner), TransferOutcome::Completed { file, .. }) =
                    (weak.upgrade(), outcome)
                {
                    let stamp = file.as_ref().map(|file| file.stamp);
                    inner.watch_and_open(remote, local, stamp);
                }
            })),
        );
    }

    fn watch_and_open(
        self: &Rc<Self>,
        remote: PathBuf,
        local: PathBuf,
        stamp: Option<RemoteStamp>,
    ) {
        // Наблюдаем за каталогом, а не за файлом: многие редакторы сохраняют
        // через временный файл и переименование.
        let monitor = local.parent().and_then(|dir| {
            gio::File::for_path(dir)
                .monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
                .map_err(|err| {
                    self.notifier.error(&tr!(
                        "Could not watch “{}” for changes: {}",
                        local.display(),
                        err
                    ));
                })
                .ok()
        });
        let watch = Rc::new(EditWatch {
            remote: RefCell::new(remote),
            remote_stamp: Cell::new(stamp),
            stamp: Cell::new(file_stamp(&local)),
            local,
            monitor,
            uploading: Cell::new(false),
            dirty: Cell::new(false),
            generation: Cell::new(0),
        });
        if let Some(monitor) = &watch.monitor {
            let weak = Rc::downgrade(self);
            let weak_watch = Rc::downgrade(&watch);
            monitor.connect_changed(move |_, _, _, _| {
                if let (Some(inner), Some(watch)) = (weak.upgrade(), weak_watch.upgrade()) {
                    inner.schedule_edit_check(&watch);
                }
            });
        }
        self.open_local(&watch.local);
        self.state.borrow_mut().edits.push(watch);
    }

    fn schedule_edit_check(self: &Rc<Self>, watch: &Rc<EditWatch>) {
        let generation = watch.generation.get().wrapping_add(1);
        watch.generation.set(generation);
        let weak = Rc::downgrade(self);
        let weak_watch = Rc::downgrade(watch);
        glib::timeout_add_local_once(EDIT_DEBOUNCE, move || {
            if let (Some(inner), Some(watch)) = (weak.upgrade(), weak_watch.upgrade())
                && watch.generation.get() == generation
            {
                inner.upload_if_changed(&watch);
            }
        });
    }

    fn upload_if_changed(self: &Rc<Self>, watch: &Rc<EditWatch>) {
        if watch.uploading.get() {
            watch.dirty.set(true);
            return;
        }
        // Файла может не быть в момент пересохранения — дождёмся следующего
        // события.
        let Some(stamp) = file_stamp(&watch.local) else {
            return;
        };
        if watch.stamp.get() == Some(stamp) {
            return;
        }
        watch.stamp.set(Some(stamp));
        // Перед заливкой сверяемся с тем, что скачали: если файл на сервере
        // успели изменить, спросим, что делать.
        let mode = match watch.remote_stamp.get() {
            Some(remote) => SaveMode::IfUnchanged(remote),
            None => SaveMode::Overwrite,
        };
        self.save_edit(watch, mode);
    }

    /// Заливает правку на сервер; пока идёт заливка (или открыт вопрос о
    /// конфликте), новые сохранения только помечают файл изменённым.
    fn save_edit(self: &Rc<Self>, watch: &Rc<EditWatch>, mode: SaveMode) {
        watch.uploading.set(true);
        let remote = watch.remote.borrow().clone();
        let name = sftp::display(Path::new(sftp::base_name(&remote)));
        let weak = Rc::downgrade(self);
        let weak_watch = Rc::downgrade(watch);
        let local = watch.local.clone();
        let target = remote.clone();
        self.start_transfer(
            &tr!("Saving {} to the server", name),
            |id| Command::UploadFile {
                id,
                local,
                remote: target,
                mode,
            },
            Some((Side::Remote, sftp::parent_remote(&remote))),
            Some(Box::new(move |outcome| {
                let (Some(inner), Some(watch)) = (weak.upgrade(), weak_watch.upgrade()) else {
                    return;
                };
                match outcome {
                    TransferOutcome::Completed { file, .. } => {
                        if let Some(file) = file {
                            watch.remote_stamp.set(Some(file.stamp));
                            if file.path != remote {
                                // Сохранили копией: дальше правка уходит в копию.
                                let copy = sftp::display(Path::new(sftp::base_name(&file.path)));
                                *watch.remote.borrow_mut() = file.path.clone();
                                inner.notifier.info(&tr!(
                                    "Saved as “{}”; further saves go to this copy",
                                    copy
                                ));
                            } else {
                                inner.notifier.info(&tr!("Saved to the server: {}", name));
                            }
                        }
                    }
                    TransferOutcome::Conflict { current } => {
                        // Решение примет пользователь; до тех пор заливка
                        // считается идущей.
                        inner.ask_edit_conflict(&watch, *current);
                        return;
                    }
                    // Не удалось: при следующем сохранении попробуем снова.
                    _ => watch.stamp.set(None),
                }
                watch.uploading.set(false);
                if watch.dirty.replace(false) {
                    inner.upload_if_changed(&watch);
                }
            })),
        );
    }

    /// Файл на сервере изменился после скачивания: перезаписать, сохранить
    /// копией рядом или не заливать.
    fn ask_edit_conflict(self: &Rc<Self>, watch: &Rc<EditWatch>, current: Option<RemoteStamp>) {
        const OVERWRITE: &str = "overwrite";
        const COPY: &str = "copy";
        const KEEP: &str = "keep";
        let remote = watch.remote.borrow().clone();
        let name = sftp::display(Path::new(sftp::base_name(&remote)));
        let copy_name = sftp::display(Path::new(&edit_copy_name(&remote)));
        let (heading, mut body, overwrite_label) = match current {
            Some(_) => (
                tr!("File Changed on the Server"),
                tr!(
                    "“{}” was changed on the server after you opened it. Your saved changes have not been uploaded yet.",
                    name
                ),
                tr!("Overwrite"),
            ),
            None => (
                tr!("File Deleted on the Server"),
                tr!(
                    "“{}” was deleted on the server after you opened it. Your saved changes have not been uploaded yet.",
                    name
                ),
                tr!("Upload Again"),
            ),
        };
        if let Some(RemoteStamp {
            modified: Some(modified),
            size,
        }) = current
        {
            body.push_str("\n\n");
            body.push_str(&tr!(
                "Server version: modified {}, {}.",
                format_mtime(modified),
                format_size(size.unwrap_or(0))
            ));
        }
        body.push_str("\n\n");
        body.push_str(&tr!(
            "“Save as Copy” puts your version next to it as “{}”.",
            copy_name
        ));

        let dialog = adw::AlertDialog::new(Some(heading), Some(&body));
        dialog.add_responses(&[
            (KEEP, tr!("Don’t Upload")),
            (COPY, tr!("Save as Copy")),
            (OVERWRITE, overwrite_label),
        ]);
        dialog.set_close_response(KEEP);
        dialog.set_default_response(Some(COPY));
        dialog.set_response_appearance(COPY, adw::ResponseAppearance::Suggested);
        if current.is_some() {
            dialog.set_response_appearance(OVERWRITE, adw::ResponseAppearance::Destructive);
        }
        let weak = Rc::downgrade(self);
        let weak_watch = Rc::downgrade(watch);
        dialog.choose(Some(&self.root), gio::Cancellable::NONE, move |response| {
            let (Some(inner), Some(watch)) = (weak.upgrade(), weak_watch.upgrade()) else {
                return;
            };
            match response.as_str() {
                OVERWRITE => inner.save_edit(&watch, SaveMode::Overwrite),
                COPY => inner.save_edit(&watch, SaveMode::Copy),
                _ => {
                    // Правка остаётся в редакторе; при следующем сохранении
                    // спросим снова.
                    watch.uploading.set(false);
                    inner.notifier.info(&tr!("Not uploaded: {}", name));
                    if watch.dirty.replace(false) {
                        inner.upload_if_changed(&watch);
                    }
                }
            }
        });
    }

    pub(super) fn open_local(&self, path: &Path) {
        let spawned = std::process::Command::new("xdg-open")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(err) => self
                .notifier
                .error(&tr!("Could not start {}: {}", "xdg-open", err)),
        }
    }
}
