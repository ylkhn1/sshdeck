//! Локальная файловая система для локального дерева панели файлов.
//!
//! Только std (и libc для `renameat2`), без GTK и `Rc`: функции блокируются
//! и вызываются из `gio::spawn_blocking`. Элементы каталога — та же
//! структура, что приходит по SFTP, с тем же смыслом полей: имя и путь
//! байтами, тип с учётом перехода по ссылке, размер и время изменения
//! (секунды Unix) — если их удалось узнать.

use std::ffi::CString;
use std::fs::{self, DirEntry, FileType, Metadata};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::entry::{EntryKind, FileEntry};

/// Читает каталог. Скрытые файлы не отбрасываются — это дело интерфейса.
/// Ссылка на каталог получает тип `Dir` (её можно раскрыть), битая
/// ссылка — `Other`; у обеих `is_link`. Если элемент не удалось
/// опросить (`lstat`), он остаётся в списке без размера и времени.
pub fn list_dir(dir: &Path) -> io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        entries.push(entry_of(&entry?));
    }
    Ok(entries)
}

fn entry_of(entry: &DirEntry) -> FileEntry {
    let path = entry.path();
    // `DirEntry::metadata` не переходит по ссылкам (`lstat`).
    let own = entry.metadata().ok();
    // Если `lstat` не удался, тип ещё может быть известен из `readdir`.
    let own_type = own
        .as_ref()
        .map(Metadata::file_type)
        .or_else(|| entry.file_type().ok());
    let is_link = own_type.is_some_and(|kind| kind.is_symlink());
    let (kind, shown) = if is_link {
        match fs::metadata(&path) {
            Ok(target) => (kind_of(target.file_type()), Some(target)),
            // Битая ссылка: размер и время — самой ссылки, как и по SFTP.
            Err(_) => (EntryKind::Other, own),
        }
    } else {
        (own_type.map_or(EntryKind::Other, kind_of), own)
    };
    FileEntry {
        name: entry.file_name(),
        path,
        kind,
        is_link,
        size: shown.as_ref().map(Metadata::len),
        modified: shown
            .as_ref()
            .and_then(|meta| u64::try_from(meta.mtime()).ok()),
        mode: shown.map(|meta| meta.mode() & 0o7777),
    }
}

fn kind_of(file_type: FileType) -> EntryKind {
    if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

/// Куда переходить по введённому пути: относительный путь берётся от
/// `cwd`, `.` и `..` убираются по тексту пути (как `cd` в оболочке), а
/// ссылки не раскрываются — `/var/home` остаётся `/var/home`. Ошибка, если
/// такого пути нет или это не каталог. `~` раскрывает интерфейс.
pub fn navigate(path: &Path, cwd: &Path) -> io::Result<PathBuf> {
    let path = clean(&cwd.join(path));
    if fs::metadata(&path)?.is_dir() {
        Ok(path)
    } else {
        Err(io::Error::from(io::ErrorKind::NotADirectory))
    }
}

/// Убирает из пути `.` и `..` по тексту; выше корня `..` не поднимается.
fn clean(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other),
        }
    }
    clean
}

/// Создаёт каталог; существующий (каталог или файл) — ошибка
/// `AlreadyExists`, ничего не заменяется.
pub fn make_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

/// Переименовывает, не заменяя существующий `to` (ни файл, ни пустой
/// каталог, который обычный `rename` заменил бы молча). Занятое имя —
/// ошибка `AlreadyExists`.
///
/// Атомарно через `renameat2(RENAME_NOREPLACE)`. Если файловая система
/// этот флаг не поддерживает (старые FUSE, NFS), занятость проверяется
/// перед обычным `rename`: остаётся лишь узкое окно между проверкой и
/// переименованием.
pub fn rename_no_clobber(from: &Path, to: &Path) -> io::Result<()> {
    match rename_noreplace(from, to) {
        Err(err) if matches!(err.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS)) => {
            if fs::symlink_metadata(to).is_ok() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            fs::rename(from, to)
        }
        result => result,
    }
}

fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: обе строки — корректные C-строки и живут до конца вызова.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Ближайший существующий каталог: сам `path` или его предок (по тексту
/// пути). `None` — если не нашлось ни одного (у относительного пути).
pub fn nearest_existing(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .filter(|dir| !dir.as_os_str().is_empty())
        .find(|dir| dir.is_dir())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn find<'a>(entries: &'a [FileEntry], name: &[u8]) -> &'a FileEntry {
        entries
            .iter()
            .find(|entry| entry.name.as_bytes() == name)
            .unwrap_or_else(|| panic!("нет {:?} в {entries:?}", OsStr::from_bytes(name)))
    }

    #[test]
    fn lists_hidden_and_non_utf8_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(".hidden"), b"x").expect("write");
        let odd = OsString::from_vec(b"\xff".to_vec());
        fs::write(dir.path().join(&odd), b"xyz").expect("write");
        let entries = list_dir(dir.path()).expect("list");
        assert_eq!(entries.len(), 2);

        let hidden = find(&entries, b".hidden");
        assert!(hidden.is_hidden());
        assert_eq!(hidden.kind, EntryKind::File);

        let odd_entry = find(&entries, b"\xff");
        assert_eq!(odd_entry.name, odd);
        assert_eq!(odd_entry.path, dir.path().join(&odd));
        assert_eq!(odd_entry.display_name(), "\u{fffd}");
        assert_eq!(odd_entry.size, Some(3));
        assert!(!odd_entry.is_link);
    }

    #[test]
    fn size_and_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file");
        fs::write(&path, vec![0u8; 1234]).expect("write");
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(mtime))
            .expect("set mtime");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).expect("chmod");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        fs::set_permissions(dir.path().join("sub"), fs::Permissions::from_mode(0o700))
            .expect("chmod");

        let entries = list_dir(dir.path()).expect("list");
        let file = find(&entries, b"file");
        assert_eq!(file.kind, EntryKind::File);
        assert_eq!(file.size, Some(1234));
        assert_eq!(file.modified, Some(1_600_000_000));
        assert_eq!(file.mode, Some(0o750));
        assert_eq!(file.path, path);
        let sub = find(&entries, b"sub");
        assert_eq!(sub.kind, EntryKind::Dir);
        assert!(!sub.is_link);
        assert!(sub.modified.is_some());
        assert_eq!(sub.mode, Some(0o700));
    }

    #[test]
    fn symlinks_to_dir_and_broken() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("target")).expect("mkdir");
        fs::write(dir.path().join("file"), b"hello").expect("write");
        symlink("target", dir.path().join("to_dir")).expect("symlink");
        symlink("file", dir.path().join("to_file")).expect("symlink");
        symlink("missing", dir.path().join("broken")).expect("symlink");

        let entries = list_dir(dir.path()).expect("list");
        let to_dir = find(&entries, b"to_dir");
        assert_eq!(to_dir.kind, EntryKind::Dir);
        assert!(to_dir.is_link);
        let to_file = find(&entries, b"to_file");
        assert_eq!(to_file.kind, EntryKind::File);
        assert!(to_file.is_link);
        // Размер — того, на что указывает ссылка.
        assert_eq!(to_file.size, Some(5));
        let broken = find(&entries, b"broken");
        assert_eq!(broken.kind, EntryKind::Other);
        assert!(broken.is_link);
        // Размер самой ссылки — длина её цели.
        assert_eq!(broken.size, Some(7));
    }

    #[test]
    fn entry_without_stat_keeps_name_and_kind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).expect("mkdir");
        fs::write(locked.join("file"), b"data").expect("write");
        // Чтение без права входа: имена видны, `lstat` — нет.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).expect("chmod");
        let stat_works = fs::symlink_metadata(locked.join("file")).is_ok();
        let entries = list_dir(&locked);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod");
        if stat_works {
            // Под root права не мешают — проверять нечего.
            return;
        }
        let entries = entries.expect("list");
        let file = find(&entries, b"file");
        assert_eq!(file.size, None);
        assert_eq!(file.modified, None);
        assert_eq!(file.mode, None);
        // Тип известен из `readdir` (d_type), если файловая система его даёт.
        assert!(matches!(file.kind, EntryKind::File | EntryKind::Other));
    }

    #[test]
    fn list_missing_dir_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(list_dir(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn navigate_cleans_and_checks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).expect("mkdir");
        fs::write(root.join("a/file"), b"").expect("write");

        assert_eq!(
            navigate(Path::new("b/../.."), &root.join("a")).unwrap(),
            root
        );
        assert_eq!(
            navigate(Path::new("./b/."), &root.join("a")).unwrap(),
            root.join("a/b")
        );
        assert_eq!(
            navigate(&root.join("a//b/../b/"), Path::new("/")).unwrap(),
            root.join("a/b")
        );
        assert_eq!(navigate(Path::new(""), root).unwrap(), root);
        assert_eq!(navigate(Path::new("/../.."), root).unwrap(), Path::new("/"));

        let err = navigate(Path::new("file"), &root.join("a")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
        let err = navigate(Path::new("missing"), root).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn navigate_keeps_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("real")).expect("mkdir");
        symlink("real", dir.path().join("link")).expect("symlink");
        assert_eq!(
            navigate(Path::new("link"), dir.path()).unwrap(),
            dir.path().join("link")
        );
    }

    #[test]
    fn make_dir_does_not_replace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("new");
        make_dir(&path).expect("mkdir");
        assert!(path.is_dir());
        let err = make_dir(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn rename_no_clobber_refuses_existing_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("from");
        let to = dir.path().join("to");
        fs::write(&from, b"new").expect("write");
        fs::write(&to, b"old").expect("write");
        let err = rename_no_clobber(&from, &to).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&from).unwrap(), b"new");
        assert_eq!(fs::read(&to).unwrap(), b"old");

        // Обычный rename молча заменил бы пустой каталог.
        let from_dir = dir.path().join("from_dir");
        let to_dir = dir.path().join("to_dir");
        fs::create_dir(&from_dir).expect("mkdir");
        fs::write(from_dir.join("inside"), b"").expect("write");
        fs::create_dir(&to_dir).expect("mkdir");
        let err = rename_no_clobber(&from_dir, &to_dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(from_dir.join("inside").exists());

        // Битая ссылка тоже занимает имя.
        let link = dir.path().join("link");
        symlink("missing", &link).expect("symlink");
        let err = rename_no_clobber(&from, &link).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn rename_no_clobber_moves_to_free_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("from");
        let to = dir.path().join(OsString::from_vec(b"to-\xff".to_vec()));
        fs::write(&from, b"data").expect("write");
        rename_no_clobber(&from, &to).expect("rename");
        assert!(!from.exists());
        assert_eq!(fs::read(&to).unwrap(), b"data");

        let from_dir = dir.path().join("dir");
        fs::create_dir(&from_dir).expect("mkdir");
        rename_no_clobber(&from_dir, &dir.path().join("renamed")).expect("rename dir");
        assert!(dir.path().join("renamed").is_dir());

        let err = rename_no_clobber(&from, &dir.path().join("x")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn nearest_existing_walks_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("mkdir");
        fs::write(root.join("a/file"), b"").expect("write");
        assert_eq!(nearest_existing(&root.join("a")), Some(root.join("a")));
        assert_eq!(
            nearest_existing(&root.join("a/x/y/z")),
            Some(root.join("a"))
        );
        // Файл — не каталог: берётся его каталог.
        assert_eq!(nearest_existing(&root.join("a/file")), Some(root.join("a")));
        assert_eq!(nearest_existing(Path::new("/")), Some(PathBuf::from("/")));
        assert_eq!(nearest_existing(Path::new("no/such/relative")), None);
    }
}
