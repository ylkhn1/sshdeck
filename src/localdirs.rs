//! Последняя локальная папка для каждого адреса:
//! `$XDG_STATE_HOME/sshdeck/local-folders.ini`
//! (по умолчанию `~/.local/state/sshdeck/`).
//!
//! Это состояние, а не настройки, поэтому лежит отдельно от
//! `connections.ini` и `settings.ini`. Ключ — адрес подключения
//! (`Session::target`), в том числе разовый `user@host`. Группы `[dir-N]`
//! идут от самой свежей; ключ `address` — адрес, `path` — папка в виде
//! URI `file://` (так сохраняются и пути не в UTF-8). Хранится не больше
//! [`MAX_FOLDERS`] адресов.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use gtk::glib;

/// Сколько адресов помнить; самые давние вытесняются.
pub const MAX_FOLDERS: usize = 100;

const FILE_NAME: &str = "local-folders.ini";
const GROUP_DIR: &str = "dir";
const KEY_ADDRESS: &str = "address";
const KEY_PATH: &str = "path";

/// Запомненные папки, от самой свежей.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalFolders {
    entries: Vec<(String, PathBuf)>,
}

/// `$XDG_STATE_HOME` по спецификации XDG: только абсолютный путь, иначе
/// `~/.local/state`. (`glib::user_state_dir` требует GLib 2.72.)
fn state_dir(xdg_state_home: Option<OsString>, home: &Path) -> PathBuf {
    xdg_state_home
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join(".local").join("state"))
}

fn folders_path() -> PathBuf {
    state_dir(std::env::var_os("XDG_STATE_HOME"), &glib::home_dir())
        .join("sshdeck")
        .join(FILE_NAME)
}

impl LocalFolders {
    /// Читает файл; отсутствующий или повреждённый даёт пустой список.
    pub fn load() -> Self {
        Self::load_from(&folders_path())
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&folders_path())
    }

    fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .map(|data| Self::parse(&data))
            .unwrap_or_default()
    }

    /// Пишет во временный файл рядом и переименовывает: файл пишется при
    /// каждом переходе и из нескольких процессов, читатель не должен
    /// увидеть его недописанным.
    fn save_to(&self, path: &Path) -> Result<(), String> {
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        let mut temp = path.as_os_str().to_owned();
        temp.push(format!(".{}.tmp", std::process::id()));
        let temp = PathBuf::from(temp);
        let result =
            std::fs::write(&temp, self.serialize()).and_then(|()| std::fs::rename(&temp, path));
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.map_err(|err| format!("{}: {err}", path.display()))
    }

    /// Последняя папка для адреса.
    pub fn get(&self, address: &str) -> Option<PathBuf> {
        self.entries
            .iter()
            .find(|(known, _)| known == address)
            .map(|(_, dir)| dir.clone())
    }

    /// Запоминает папку для адреса и переносит его в начало списка.
    /// Пустой адрес и относительный путь не запоминаются.
    pub fn remember(&mut self, address: &str, dir: &Path) {
        if address.is_empty() || !dir.is_absolute() {
            return;
        }
        self.entries.retain(|(known, _)| known != address);
        self.entries
            .insert(0, (address.to_string(), dir.to_path_buf()));
        self.entries.truncate(MAX_FOLDERS);
    }

    fn parse(data: &str) -> Self {
        let mut folders = Self::default();
        let key_file = glib::KeyFile::new();
        if key_file
            .load_from_data(data, glib::KeyFileFlags::NONE)
            .is_err()
        {
            return folders;
        }
        for group in key_file.groups().iter() {
            let group = group.as_str();
            if !group.starts_with(GROUP_DIR) {
                continue;
            }
            let Ok(address) = key_file.string(group, KEY_ADDRESS) else {
                continue;
            };
            let Ok(uri) = key_file.string(group, KEY_PATH) else {
                continue;
            };
            let Ok((dir, _)) = glib::filename_from_uri(uri.as_str()) else {
                continue;
            };
            // Повторы адреса: верна первая (самая свежая) запись.
            if folders.get(address.as_str()).is_none() {
                folders.remember_last(address.as_str(), &dir);
            }
        }
        folders
    }

    /// Добавляет в конец (при чтении: файл идёт от свежих к давним).
    fn remember_last(&mut self, address: &str, dir: &Path) {
        if self.entries.len() < MAX_FOLDERS && !address.is_empty() && dir.is_absolute() {
            self.entries.push((address.to_string(), dir.to_path_buf()));
        }
    }

    fn serialize(&self) -> String {
        let key_file = glib::KeyFile::new();
        let uris = self
            .entries
            .iter()
            .filter_map(|(address, dir)| Some((address, glib::filename_to_uri(dir, None).ok()?)));
        for (index, (address, uri)) in uris.enumerate() {
            let group = format!("{GROUP_DIR}-{}", index + 1);
            key_file.set_string(&group, KEY_ADDRESS, address);
            key_file.set_string(&group, KEY_PATH, &uri);
        }
        key_file.to_data().to_string()
    }
}

/// С какой папки открыть локальное дерево: сохранённая, если она есть;
/// иначе ближайший существующий её предок (кроме корня — тогда уж
/// домашняя папка); иначе домашняя. `exists` — «это существующий каталог»;
/// на живой системе его стоит вызывать не в главном потоке.
pub fn restore_dir(saved: Option<&Path>, home: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    saved
        .filter(|saved| saved.is_absolute())
        .and_then(|saved| {
            let is_root = saved.parent().is_none();
            saved
                .ancestors()
                .filter(|dir| is_root || dir.parent().is_some())
                .find(|dir| exists(dir))
        })
        .map_or_else(|| home.to_path_buf(), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    fn odd_path() -> PathBuf {
        PathBuf::from(OsString::from_vec(
            b"/home/u/\xff\xfe dir/%20;#=[x]".to_vec(),
        ))
    }

    #[test]
    fn round_trip_with_non_utf8_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("state/sshdeck").join(FILE_NAME);
        let mut folders = LocalFolders::default();
        folders.remember("user@host", Path::new("/srv/data"));
        folders.remember("db = [prod]; #1", &odd_path());
        folders.save_to(&file).expect("save");

        let data = std::fs::read_to_string(&file).expect("read");
        assert!(
            data.contains("[dir-1]\naddress=db = [prod]; #1\npath=file:///home/u/%FF%FE"),
            "{data}"
        );
        assert!(
            data.contains("[dir-2]\naddress=user@host\npath=file:///srv/data\n"),
            "{data}"
        );

        let loaded = LocalFolders::load_from(&file);
        assert_eq!(loaded, folders);
        assert_eq!(loaded.get("db = [prod]; #1"), Some(odd_path()));
        assert_eq!(loaded.get("user@host"), Some(PathBuf::from("/srv/data")));
        assert_eq!(loaded.get("other"), None);
        // Временный файл не остаётся.
        let names: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [FILE_NAME]);
    }

    #[test]
    fn keeps_most_recent_hundred() {
        let mut folders = LocalFolders::default();
        for index in 0..150 {
            folders.remember(&format!("host{index}"), Path::new(&format!("/d/{index}")));
        }
        let folders = LocalFolders::parse(&folders.serialize());
        assert_eq!(folders.entries.len(), MAX_FOLDERS);
        assert_eq!(folders.entries[0].0, "host149");
        assert_eq!(folders.entries[99].0, "host50");
        assert_eq!(folders.get("host49"), None);
        assert_eq!(folders.get("host120"), Some(PathBuf::from("/d/120")));
    }

    #[test]
    fn remembering_again_moves_to_front() {
        let mut folders = LocalFolders::default();
        folders.remember("a", Path::new("/a1"));
        folders.remember("b", Path::new("/b"));
        folders.remember("a", Path::new("/a2"));
        assert_eq!(
            folders.entries,
            [
                ("a".to_string(), PathBuf::from("/a2")),
                ("b".to_string(), PathBuf::from("/b")),
            ]
        );
        // Пустой адрес и относительный путь не запоминаются.
        folders.remember("", Path::new("/x"));
        folders.remember("c", Path::new("relative"));
        assert_eq!(folders.entries.len(), 2);
    }

    #[test]
    fn hand_edited_file_is_cleaned() {
        let data = "[dir-1]\naddress=a\npath=file:///first\n\
                    [dir-2]\naddress=a\npath=file:///duplicate\n\
                    [dir-3]\naddress=b\npath=not a uri\n\
                    [dir-4]\npath=file:///no-address\n\
                    [dir-5]\naddress=c\n\
                    [other]\naddress=d\npath=file:///other\n\
                    [dir-6]\naddress=e\npath=file:///e\n";
        let folders = LocalFolders::parse(data);
        assert_eq!(
            folders.entries,
            [
                ("a".to_string(), PathBuf::from("/first")),
                ("e".to_string(), PathBuf::from("/e")),
            ]
        );
    }

    #[test]
    fn corrupt_or_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            LocalFolders::load_from(&dir.path().join("missing.ini")),
            LocalFolders::default()
        );
        let corrupt = dir.path().join("corrupt.ini");
        std::fs::write(&corrupt, b"[dir-1\naddress=a\n\xff\x00garbage").expect("write");
        assert_eq!(LocalFolders::load_from(&corrupt), LocalFolders::default());
        assert_eq!(
            LocalFolders::parse("not ini at all"),
            LocalFolders::default()
        );
        // Каталог вместо файла — тоже пусто.
        assert_eq!(LocalFolders::load_from(dir.path()), LocalFolders::default());
    }

    #[test]
    fn state_dir_follows_xdg() {
        let home = Path::new("/home/u");
        let xdg = |value: &str| Some(OsString::from(value));
        assert_eq!(state_dir(xdg("/var/state"), home), Path::new("/var/state"));
        // Относительный путь спецификация велит игнорировать.
        assert_eq!(
            state_dir(xdg("state"), home),
            Path::new("/home/u/.local/state")
        );
        assert_eq!(state_dir(xdg(""), home), Path::new("/home/u/.local/state"));
        assert_eq!(state_dir(None, home), Path::new("/home/u/.local/state"));
    }

    #[test]
    fn restore_dir_falls_back() {
        let home = Path::new("/home/u");
        let existing = ["/", "/srv", "/srv/data", "/home/u"];
        let exists = |dir: &Path| existing.iter().any(|known| Path::new(known) == dir);

        let saved = Path::new("/srv/data");
        assert_eq!(restore_dir(Some(saved), home, exists), saved);
        let gone = Path::new("/srv/data/old/deeper");
        assert_eq!(
            restore_dir(Some(gone), home, exists),
            Path::new("/srv/data")
        );
        // Пропала вся ветка — не в корень, а домой.
        let unmounted = Path::new("/media/usb/photos");
        assert_eq!(restore_dir(Some(unmounted), home, exists), home);
        // Корень, если он и был сохранён.
        assert_eq!(
            restore_dir(Some(Path::new("/")), home, exists),
            Path::new("/")
        );
        assert_eq!(restore_dir(None, home, exists), home);
        assert_eq!(restore_dir(Some(Path::new("relative")), home, exists), home);
    }
}
