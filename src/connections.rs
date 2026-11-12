//! Сохранённые подключения: `~/.config/sshdeck/connections.ini`.
//!
//! У подключения есть название, адрес (в том же виде, что и в поле быстрого
//! подключения) и необязательная папка. Паролей и ключей здесь нет: вход
//! выполняет системный ssh.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gtk::glib;

const GROUP_PREFIX: &str = "connection";
const KEY_NAME: &str = "name";
const KEY_ADDRESS: &str = "address";
const KEY_FOLDER: &str = "folder";
const KEY_STARTUP: &str = "startup";
const FOLDER_GROUP_PREFIX: &str = "folder";
const KEY_COLOR: &str = "color";

/// Цвета, которыми можно пометить папку: идентификатор в файле и название
/// в меню.
pub const FOLDER_COLORS: [(&str, &str); 7] = [
    ("red", n_!("Red")),
    ("orange", n_!("Orange")),
    ("yellow", n_!("Yellow")),
    ("green", n_!("Green")),
    ("blue", n_!("Blue")),
    ("purple", n_!("Purple")),
    ("brown", n_!("Brown")),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// Название в списке и на вкладке.
    pub name: String,
    /// Адрес: `user@host`, `user@host -p 2222`, псевдоним из ssh-конфига.
    pub address: String,
    /// Папка; пустая строка — подключение лежит вне папок.
    pub folder: String,
    /// Команда, которая отправляется в терминал сразу после входа.
    pub startup: String,
}

impl Connection {
    /// Приводит поля к хранимому виду: одна строка без пробелов по краям;
    /// пустое название заменяется адресом.
    pub fn new(name: &str, address: &str, folder: &str) -> Self {
        let address = clean(address);
        let name = match clean(name) {
            name if name.is_empty() => address.clone(),
            name => name,
        };
        Self {
            name,
            address,
            folder: clean(folder),
            startup: String::new(),
        }
    }

    /// Добавляет команду после входа (одной строкой, пробелы внутри
    /// сохраняются).
    pub fn with_startup(mut self, command: &str) -> Self {
        self.startup = command
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>()
            .trim()
            .to_string();
        self
    }
}

/// Одна строка: управляющие символы и повторные пробелы схлопываются.
pub fn clean(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Список подключений, всегда упорядоченный: сначала без папки, затем по
/// папкам; внутри — по названию.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connections {
    items: Vec<Connection>,
    /// Цвет папки по её названию (идентификатор из [`FOLDER_COLORS`]).
    colors: BTreeMap<String, String>,
}

pub fn default_path() -> PathBuf {
    glib::user_config_dir()
        .join("sshdeck")
        .join("connections.ini")
}

impl Connections {
    /// Читает файл. Отсутствующий файл — это пустой список, а не ошибка.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(data) => Self::parse(&data).map_err(|err| format!("{}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(format!("{}: {err}", path.display())),
        }
    }

    pub fn parse(data: &str) -> Result<Self, String> {
        let key_file = glib::KeyFile::new();
        key_file
            .load_from_data(data, glib::KeyFileFlags::NONE)
            .map_err(|err| err.to_string())?;
        let mut items = Vec::new();
        let mut colors = BTreeMap::new();
        for group in key_file.groups().iter() {
            let group = group.as_str();
            let field = |key: &str| {
                key_file
                    .string(group, key)
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            };
            if group.starts_with(FOLDER_GROUP_PREFIX) {
                let name = clean(&field(KEY_FOLDER));
                let color = field(KEY_COLOR);
                if !name.is_empty() && FOLDER_COLORS.iter().any(|(id, _)| *id == color) {
                    colors.insert(name, color);
                }
                continue;
            }
            if !group.starts_with(GROUP_PREFIX) {
                continue;
            }
            let connection = Connection::new(&field(KEY_NAME), &field(KEY_ADDRESS), &field(KEY_FOLDER))
                .with_startup(&field(KEY_STARTUP));
            // Запись без адреса бесполезна: подключаться некуда.
            if !connection.address.is_empty() {
                items.push(connection);
            }
        }
        let mut this = Self { items, colors };
        this.sort();
        Ok(this)
    }

    pub fn serialize(&self) -> String {
        let key_file = glib::KeyFile::new();
        for (index, item) in self.items.iter().enumerate() {
            let group = format!("{GROUP_PREFIX}-{}", index + 1);
            key_file.set_string(&group, KEY_NAME, &item.name);
            key_file.set_string(&group, KEY_ADDRESS, &item.address);
            if !item.folder.is_empty() {
                key_file.set_string(&group, KEY_FOLDER, &item.folder);
            }
            if !item.startup.is_empty() {
                key_file.set_string(&group, KEY_STARTUP, &item.startup);
            }
        }
        // Цвета папок, в которых не осталось подключений, не сохраняются.
        let mut index = 0;
        for folder in self.folders() {
            if let Some(color) = self.colors.get(&folder) {
                index += 1;
                let group = format!("{FOLDER_GROUP_PREFIX}-{index}");
                key_file.set_string(&group, KEY_FOLDER, &folder);
                key_file.set_string(&group, KEY_COLOR, color);
            }
        }
        key_file.to_data().to_string()
    }

    /// Записывает файл через временный, чтобы сбой не оставил половину
    /// списка.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let describe = |target: &Path, err: std::io::Error| format!("{}: {err}", target.display());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| describe(dir, err))?;
        }
        let mut temp = path.as_os_str().to_owned();
        temp.push(".tmp");
        let temp = PathBuf::from(temp);
        std::fs::write(&temp, self.serialize()).map_err(|err| describe(&temp, err))?;
        std::fs::rename(&temp, path).map_err(|err| describe(path, err))
    }

    pub fn items(&self) -> &[Connection] {
        &self.items
    }

    pub fn get(&self, index: usize) -> Option<&Connection> {
        self.items.get(index)
    }

    /// Названия папок по порядку, без повторов.
    pub fn folders(&self) -> Vec<String> {
        let mut folders: Vec<String> = Vec::new();
        for item in &self.items {
            if !item.folder.is_empty() && folders.last() != Some(&item.folder) {
                folders.push(item.folder.clone());
            }
        }
        folders
    }

    pub fn add(&mut self, connection: Connection) {
        self.items.push(connection);
        self.sort();
    }

    /// Заменяет запись; `false`, если такой уже нет.
    pub fn replace(&mut self, index: usize, connection: Connection) -> bool {
        let Some(slot) = self.items.get_mut(index) else {
            return false;
        };
        *slot = connection;
        self.sort();
        true
    }

    pub fn remove(&mut self, index: usize) -> Option<Connection> {
        (index < self.items.len()).then(|| self.items.remove(index))
    }

    /// Переносит все подключения из папки `from` в `to` (пустая строка —
    /// вынести из папок). Возвращает число затронутых записей.
    pub fn move_folder(&mut self, from: &str, to: &str) -> usize {
        let to = clean(to);
        let mut moved = 0;
        for item in self.items.iter_mut().filter(|item| item.folder == from) {
            item.folder = to.clone();
            moved += 1;
        }
        self.sort();
        // Цвет переезжает вместе с папкой, если у новой папки нет своего.
        if moved > 0
            && let Some(color) = self.colors.remove(from)
            && !to.is_empty()
        {
            self.colors.entry(to).or_insert(color);
        }
        moved
    }

    /// Цвет папки, если он задан.
    pub fn folder_color(&self, folder: &str) -> Option<&str> {
        self.colors.get(folder).map(String::as_str)
    }

    /// Задаёт цвет папки; `None` или неизвестный идентификатор убирают его.
    pub fn set_folder_color(&mut self, folder: &str, color: Option<&str>) {
        match color.filter(|color| FOLDER_COLORS.iter().any(|(id, _)| id == color)) {
            Some(color) => {
                self.colors.insert(folder.to_string(), color.to_string());
            }
            None => {
                self.colors.remove(folder);
            }
        }
    }

    fn sort(&mut self) {
        self.items.sort_by_cached_key(|item| {
            (
                item.folder.to_lowercase(),
                item.folder.clone(),
                item.name.to_lowercase(),
                item.name.clone(),
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{Connection, Connections, clean};

    fn sample() -> Connections {
        let mut list = Connections::default();
        list.add(Connection::new("web", "root@10.0.0.2", "Прод"));
        list.add(Connection::new("Домашний", "me@home -p 2222", ""));
        list.add(Connection::new("db", "root@10.0.0.3", "Прод"));
        list.add(Connection::new("стенд", "dev@stand", "Тест"));
        list
    }

    fn names(list: &Connections) -> Vec<&str> {
        list.items().iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn fields_are_cleaned() {
        let c = Connection::new("  Мой\tсервер \n", " u@h  -p 22 ", " Папка ");
        assert_eq!(c.name, "Мой сервер");
        assert_eq!(c.address, "u@h -p 22");
        assert_eq!(c.folder, "Папка");
        assert_eq!(clean("   "), "");
    }

    #[test]
    fn startup_command_is_kept_and_saved() {
        let c = Connection::new("x", "u@h", "").with_startup("  cd /srv  &&  ls\n");
        assert_eq!(c.startup, "cd /srv  &&  ls");
        let mut list = Connections::default();
        list.add(c.clone());
        let restored = Connections::parse(&list.serialize()).expect("parse");
        assert_eq!(restored.items(), [c]);
    }

    #[test]
    fn empty_name_falls_back_to_address() {
        assert_eq!(Connection::new(" ", "u@h", "").name, "u@h");
    }

    #[test]
    fn order_is_root_first_then_folders_then_names() {
        let list = sample();
        assert_eq!(names(&list), ["Домашний", "db", "web", "стенд"]);
        assert_eq!(list.folders(), ["Прод", "Тест"]);
    }

    #[test]
    fn round_trip_keeps_everything() {
        let mut list = sample();
        list.add(Connection::new("хитрое; имя = [x] # y", "u@h", "a;b"));
        let restored = Connections::parse(&list.serialize()).expect("parse");
        assert_eq!(restored, list);
    }

    #[test]
    fn records_without_address_are_dropped() {
        let data = "[connection-1]\nname=x\n\n[connection-2]\nname=y\naddress=u@h\n\n[other]\na=b\n";
        let list = Connections::parse(data).expect("parse");
        assert_eq!(names(&list), ["y"]);
    }

    #[test]
    fn broken_file_is_an_error() {
        assert!(Connections::parse("это не ini").is_err());
    }

    #[test]
    fn replace_and_remove() {
        let mut list = sample();
        assert!(list.replace(1, Connection::new("архив", "root@10.0.0.3", "")));
        assert_eq!(names(&list), ["архив", "Домашний", "web", "стенд"]);
        assert!(!list.replace(99, Connection::new("x", "y", "")));
        assert_eq!(list.remove(0).map(|c| c.name), Some("архив".to_string()));
        assert!(list.remove(99).is_none());
        assert_eq!(list.items().len(), 3);
    }

    #[test]
    fn folders_can_be_renamed_merged_and_dissolved() {
        let mut list = sample();
        assert_eq!(list.move_folder("Прод", "Тест"), 2);
        assert_eq!(list.folders(), ["Тест"]);
        assert_eq!(list.move_folder("Тест", ""), 3);
        assert!(list.folders().is_empty());
        assert_eq!(list.move_folder("нет такой", "x"), 0);
    }

    #[test]
    fn folder_colors_are_saved_and_follow_the_folder() {
        let mut list = sample();
        list.set_folder_color("Прод", Some("red"));
        list.set_folder_color("Тест", Some("no-such-color"));
        assert_eq!(list.folder_color("Прод"), Some("red"));
        assert_eq!(list.folder_color("Тест"), None);

        let restored = Connections::parse(&list.serialize()).expect("parse");
        assert_eq!(restored.folder_color("Прод"), Some("red"));

        list.move_folder("Прод", "Боевые");
        assert_eq!(list.folder_color("Боевые"), Some("red"));
        assert_eq!(list.folder_color("Прод"), None);

        list.set_folder_color("Боевые", None);
        assert_eq!(list.folder_color("Боевые"), None);
    }

    #[test]
    fn colors_of_vanished_folders_are_not_saved() {
        let mut list = sample();
        list.set_folder_color("Тест", Some("blue"));
        list.move_folder("Тест", "");
        list.set_folder_color("Призрак", Some("green"));
        assert!(!list.serialize().contains("color"));
    }

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("connections.ini");
        assert_eq!(Connections::load(&path), Ok(Connections::default()));
        let list = sample();
        list.save(&path).expect("save");
        assert_eq!(Connections::load(&path), Ok(list));
        assert!(!path.with_extension("ini.tmp").exists());
    }
}
