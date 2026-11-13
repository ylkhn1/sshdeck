//! Перетаскивание строк дерева: между деревьями панели и наружу.
//!
//! Данные перетаскивания — объединение двух источников. Для приложений
//! снаружи: локальные файлы списком (`gdk::FileList`) или, для файлов
//! сервера, ленивое скачивание во временный каталог
//! ([`RemoteFilesProvider`](super::super::dnd::RemoteFilesProvider)).
//! Для деревьев sshdeck — [`FileDrag`]: что и откуда тащат. Приёмник
//! панели сначала просит `FileDrag`, поэтому файлы сервера, брошенные на
//! локальное дерево, во временный каталог не скачиваются — их сразу
//! скачивают куда бросили.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::Inner;
use super::tree::Side;
use crate::sftp::FileEntry;

/// Что тащат из дерева панели: снимок выделения на начало перетаскивания.
#[derive(Debug, Clone, glib::Boxed)]
#[boxed_type(name = "SshdeckFileDrag")]
pub(super) struct FileDrag {
    /// Номер панели-источника.
    pub(super) panel: u64,
    pub(super) side: Side,
    pub(super) entries: Vec<FileEntry>,
}

thread_local! {
    /// Перетаскивание из дерева какой-нибудь панели, идущее сейчас:
    /// приёмнику нужно знать источник ещё до броска, а данные до броска
    /// не читаются.
    static CURRENT: Cell<Option<(u64, Side)>> = const { Cell::new(None) };
}

/// Перетаскивание из дерева началось.
fn begin(panel: u64, side: Side) {
    CURRENT.with(|current| current.set(Some((panel, side))));
}

/// Перетаскивание из дерева закончилось (брошено или отменено).
pub(super) fn end() {
    CURRENT.with(|current| current.set(None));
}

/// Откуда тащат.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DragOrigin {
    /// Из другого приложения (или другого процесса sshdeck).
    External,
    /// Из дерева панели этого процесса.
    Tree { panel: u64, side: Side },
    /// Из этого же приложения, но не из дерева (вкладка, текст).
    OtherInApp,
}

/// Что сделает бросок.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Transfer {
    Upload,
    Download,
}

/// Что сделает бросок на дерево `target` панели `panel`; `None` — бросок
/// не принимается. На сервер — локальные файлы откуда угодно (из
/// файлового менеджера или локального дерева любой вкладки); в локальное
/// дерево — только файлы сервера из этой же панели (у другой вкладки
/// может быть другой сервер). Обоим направлениям нужен SFTP.
pub(super) fn drop_action(
    target: Side,
    origin: DragOrigin,
    panel: u64,
    connected: bool,
) -> Option<Transfer> {
    let transfer = match (target, origin) {
        (Side::Remote, DragOrigin::External)
        | (
            Side::Remote,
            DragOrigin::Tree {
                side: Side::Local, ..
            },
        ) => Transfer::Upload,
        (
            Side::Local,
            DragOrigin::Tree {
                panel: from,
                side: Side::Remote,
            },
        ) if from == panel => Transfer::Download,
        _ => return None,
    };
    connected.then_some(transfer)
}

/// Локальные файлы для приложений снаружи (`text/uri-list` и прочее,
/// что GTK умеет делать из `gdk::FileList`).
fn local_files_provider(entries: &[FileEntry]) -> gdk::ContentProvider {
    let files: Vec<gio::File> = entries
        .iter()
        .map(|entry| gio::File::for_path(&entry.path))
        .collect();
    gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value())
}

/// Данные для приложений снаружи плюс [`FileDrag`] для деревьев панели;
/// `FileDrag` идёт первым.
fn with_file_drag(drag: FileDrag, outside: gdk::ContentProvider) -> gdk::ContentProvider {
    let inside = gdk::ContentProvider::for_value(&drag.to_value());
    gdk::ContentProvider::new_union(&[inside, outside])
}

fn origin_of(target: &gtk::DropTarget) -> DragOrigin {
    let local = target
        .current_drop()
        .is_some_and(|drop| drop.drag().is_some());
    if !local {
        return DragOrigin::External;
    }
    CURRENT
        .with(Cell::get)
        .map_or(DragOrigin::OtherInApp, |(panel, side)| DragOrigin::Tree {
            panel,
            side,
        })
}

impl Inner {
    /// Что сделает бросок, идущий на дерево `side`.
    pub(super) fn drop_transfer(&self, target: &gtk::DropTarget, side: Side) -> Option<Transfer> {
        drop_action(side, origin_of(target), self.id, self.connected.get())
    }

    /// Бросок принят: `dest` — каталог под указателем (иначе текущий).
    pub(super) fn accept_drop(
        self: &Rc<Self>,
        side: Side,
        value: &glib::Value,
        dest: Option<PathBuf>,
    ) -> bool {
        if let Ok(drag) = value.get::<FileDrag>() {
            return match (side, drag.side) {
                (Side::Remote, Side::Local) => {
                    let paths = drag.entries.into_iter().map(|entry| entry.path).collect();
                    self.upload_paths(paths, dest);
                    true
                }
                (Side::Local, Side::Remote) if drag.panel == self.id => {
                    let Some(local) = self.local.get() else {
                        return false;
                    };
                    let dest = dest.unwrap_or_else(|| local.tree.cwd());
                    if dest.as_os_str().is_empty() {
                        return false;
                    }
                    self.download_to(drag.entries, dest);
                    true
                }
                _ => false,
            };
        }
        let Ok(list) = value.get::<gdk::FileList>() else {
            return false;
        };
        if side != Side::Remote {
            return false;
        }
        let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
        if paths.is_empty() {
            return false;
        }
        self.upload_paths(paths, dest);
        true
    }

    /// Данные перетаскивания выделения дерева `side`.
    pub(super) fn drag_content(self: &Rc<Self>, side: Side) -> Option<gdk::ContentProvider> {
        let entries = self.tree(side).selected_transferable();
        if entries.is_empty() {
            return None;
        }
        let outside = match side {
            Side::Remote => self.remote_files_provider(entries.clone()).upcast(),
            Side::Local => local_files_provider(&entries),
        };
        let drag = FileDrag {
            panel: self.id,
            side,
            entries,
        };
        begin(self.id, side);
        Some(with_file_drag(drag, outside))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_action_table() {
        use DragOrigin::*;
        use Side::{Local, Remote};
        use Transfer::{Download, Upload};
        const PANEL: u64 = 7;
        const OTHER: u64 = 8;
        let tree = |panel, side| Tree { panel, side };
        // (цель, источник, SFTP есть, ожидание)
        let table = [
            (Remote, External, true, Some(Upload)),
            (Remote, tree(PANEL, Local), true, Some(Upload)),
            // Локальное дерево другой вкладки — те же локальные файлы.
            (Remote, tree(OTHER, Local), true, Some(Upload)),
            (Remote, tree(PANEL, Remote), true, None),
            (Remote, tree(OTHER, Remote), true, None),
            (Remote, OtherInApp, true, None),
            (Local, tree(PANEL, Remote), true, Some(Download)),
            // Сервер другой вкладки может быть другим сервером.
            (Local, tree(OTHER, Remote), true, None),
            (Local, tree(PANEL, Local), true, None),
            (Local, External, true, None),
            (Local, OtherInApp, true, None),
            // Без SFTP не принимается ничего.
            (Remote, External, false, None),
            (Remote, tree(PANEL, Local), false, None),
            (Local, tree(PANEL, Remote), false, None),
        ];
        for (target, origin, connected, expected) in table {
            assert_eq!(
                drop_action(target, origin, PANEL, connected),
                expected,
                "{target:?} <- {origin:?}, connected={connected}"
            );
        }
    }

    #[test]
    fn local_drag_offers_file_drag_and_file_list() {
        let entries = vec![super::super::test_entry(
            "/home/me/a.txt",
            crate::sftp::EntryKind::File,
        )];
        let drag = FileDrag {
            panel: 1,
            side: Side::Local,
            entries: entries.clone(),
        };
        let provider = with_file_drag(drag, local_files_provider(&entries));
        let formats = provider.formats();
        assert!(formats.contains_type(FileDrag::static_type()));
        // Из `gdk::FileList` GTK сам делает `text/uri-list` для файловых
        // менеджеров (сериализаторы без дисплея в тестах не работают).
        assert!(formats.contains_type(gdk::FileList::static_type()));
        let value = provider
            .value(FileDrag::static_type())
            .expect("FileDrag из объединения");
        assert_eq!(value.get::<FileDrag>().unwrap().entries, entries);
    }

    #[test]
    fn file_drag_survives_a_value() {
        let drag = FileDrag {
            panel: 3,
            side: Side::Remote,
            entries: vec![super::super::test_entry(
                "/srv/a",
                crate::sftp::EntryKind::File,
            )],
        };
        let value = drag.to_value();
        let back = value.get::<FileDrag>().expect("FileDrag");
        assert_eq!(back.panel, 3);
        assert_eq!(back.side, Side::Remote);
        assert_eq!(back.entries, drag.entries);
        assert!(value.get::<gdk::FileList>().is_err());
    }
}
