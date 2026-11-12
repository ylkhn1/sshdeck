//! Сохранённые подключения к базам данных: `~/.config/sshdeck/databases.ini`.
//!
//! Паролей здесь нет: их спрашивает окно при подключении и, если попросить,
//! держит в памяти до закрытия программы.

use std::path::{Path, PathBuf};

use gtk::glib;

use crate::connections::clean;

const GROUP_PREFIX: &str = "database";
const KEY_NAME: &str = "name";
const KEY_KIND: &str = "kind";
const KEY_HOST: &str = "host";
const KEY_PORT: &str = "port";
const KEY_USER: &str = "user";
const KEY_DATABASE: &str = "database";
const KEY_PATH: &str = "path";
const KEY_SSH: &str = "ssh";
const KEY_TLS: &str = "tls";
const KEY_DRIVER: &str = "driver";

/// Поддерживаемые СУБД. `Extension` — СУБД из установленного расширения
/// (какого именно, говорит [`DbProfile::driver`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DbKind {
    Postgres,
    MySql,
    Sqlite,
    Extension,
}

impl DbKind {
    /// Встроенные драйверы.
    pub const ALL: [DbKind; 3] = [DbKind::Postgres, DbKind::MySql, DbKind::Sqlite];

    /// Идентификатор в файле настроек.
    pub fn id(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgres",
            DbKind::MySql => "mysql",
            DbKind::Sqlite => "sqlite",
            DbKind::Extension => "extension",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .chain([DbKind::Extension])
            .find(|kind| kind.id() == id)
    }

    /// Название для людей.
    pub fn label(self) -> &'static str {
        match self {
            DbKind::Postgres => "PostgreSQL",
            DbKind::MySql => "MySQL / MariaDB",
            DbKind::Sqlite => "SQLite",
            DbKind::Extension => tr!("Extension"),
        }
    }

    /// Короткая метка для значка в списке.
    pub fn badge(self) -> &'static str {
        match self {
            DbKind::Postgres => "PG",
            DbKind::MySql => "MY",
            DbKind::Sqlite => "SL",
            DbKind::Extension => "EX",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            DbKind::Postgres => 5432,
            DbKind::MySql => 3306,
            DbKind::Sqlite | DbKind::Extension => 0,
        }
    }

    pub fn default_user(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgres",
            DbKind::MySql => "root",
            DbKind::Sqlite | DbKind::Extension => "",
        }
    }

    /// Сетевая СУБД (в отличие от файла SQLite): у неё есть сервер, порт,
    /// пароль и её можно открыть через SSH-туннель.
    pub fn is_networked(self) -> bool {
        !matches!(self, DbKind::Sqlite)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DbProfile {
    pub name: String,
    pub kind: DbKind,
    /// Сервер СУБД. При подключении через SSH — адрес так, как его видит
    /// SSH-сервер (обычно `localhost`).
    pub host: String,
    pub port: u16,
    pub user: String,
    /// База по умолчанию; может быть пустой.
    pub database: String,
    /// Файл базы SQLite.
    pub path: String,
    /// SSH-сервер, через который пробрасывается соединение (адрес как в
    /// списке подключений); пустая строка — подключаться напрямую.
    pub ssh: String,
    /// Требовать шифрование соединения.
    pub tls: bool,
    /// Идентификатор расширения для `DbKind::Extension`.
    pub driver: String,
}

impl DbProfile {
    /// Профиль с типовыми значениями для выбранной СУБД.
    pub fn template(kind: DbKind) -> Self {
        Self::template_for(kind, String::new())
    }

    /// Профиль для СУБД из расширения `driver`.
    pub fn template_for(kind: DbKind, driver: String) -> Self {
        let mut profile = Self {
            name: String::new(),
            kind,
            host: String::new(),
            port: 0,
            user: String::new(),
            database: String::new(),
            path: String::new(),
            ssh: String::new(),
            tls: false,
            driver,
        };
        if profile.is_networked() {
            profile.host = "localhost".to_string();
        }
        profile.port = profile.default_port();
        profile.user = profile.default_user();
        profile
    }

    /// Расширение профиля, если оно установлено.
    pub fn extension(&self) -> Option<super::extension::ExtensionInfo> {
        (self.kind == DbKind::Extension)
            .then(|| super::extension::find(&self.driver))
            .flatten()
    }

    /// Сетевая СУБД: сервер, порт, пароль, SSH-туннель. Для расширения это
    /// говорит само расширение (неустановленное считается сетевым).
    pub fn is_networked(&self) -> bool {
        match self.kind {
            DbKind::Extension => self.extension().is_none_or(|ext| !ext.file),
            kind => kind.is_networked(),
        }
    }

    pub fn default_port(&self) -> u16 {
        match self.kind {
            DbKind::Extension => self.extension().map_or(0, |ext| ext.default_port),
            kind => kind.default_port(),
        }
    }

    pub fn default_user(&self) -> String {
        match self.kind {
            DbKind::Extension => self
                .extension()
                .map(|ext| ext.default_user)
                .unwrap_or_default(),
            kind => kind.default_user().to_string(),
        }
    }

    /// Название СУБД для людей.
    pub fn kind_label(&self) -> String {
        match self.kind {
            DbKind::Extension => self.extension().map_or_else(
                || tr!("{} (extension not installed)", self.driver),
                |ext| ext.name,
            ),
            kind => kind.label().to_string(),
        }
    }

    /// Короткая метка для значка.
    pub fn badge(&self) -> String {
        match self.kind {
            DbKind::Extension => self.extension().map_or_else(
                || {
                    self.driver
                        .chars()
                        .take(2)
                        .collect::<String>()
                        .to_uppercase()
                },
                |ext| ext.badge,
            ),
            kind => kind.badge().to_string(),
        }
    }

    /// Приводит поля к хранимому виду; пустое название заменяется кратким
    /// описанием.
    pub fn normalized(mut self) -> Self {
        self.host = clean(&self.host);
        self.user = clean(&self.user);
        self.database = clean(&self.database);
        self.path = self.path.trim().to_string();
        self.ssh = clean(&self.ssh);
        self.name = clean(&self.name);
        self.driver = clean(&self.driver);
        if self.port == 0 {
            self.port = self.default_port();
        }
        if self.name.is_empty() {
            self.name = self.summary();
        }
        self
    }

    /// Куда подключаемся, одной строкой: `postgres@db:5432/app`, имя файла.
    pub fn summary(&self) -> String {
        if !self.is_networked() {
            return Path::new(&self.path).file_name().map_or_else(
                || self.path.clone(),
                |name| name.to_string_lossy().into_owned(),
            );
        }
        let mut text = String::new();
        if !self.user.is_empty() {
            text.push_str(&self.user);
            text.push('@');
        }
        text.push_str(&self.host);
        if self.port != self.default_port() {
            text.push_str(&format!(":{}", self.port));
        }
        if !self.database.is_empty() {
            text.push('/');
            text.push_str(&self.database);
        }
        text
    }

    /// Чего не хватает, чтобы подключиться; `None` — профиль годится.
    pub fn problem(&self) -> Option<String> {
        if self.kind == DbKind::Extension && self.extension().is_none() {
            return Some(tr!(
                "Extension “{}” is not installed. Put it in {}",
                self.driver,
                super::extension::drivers_dir().display()
            ));
        }
        if self.is_networked() {
            if self.host.trim().is_empty() {
                return Some(tr!("Enter the database server").to_string());
            }
            if !self.ssh.trim().is_empty()
                && let Err(message) = crate::session::validate_target(&self.ssh)
            {
                return Some(tr!("SSH server: {}", message));
            }
        } else if self.path.trim().is_empty() {
            return Some(tr!("Choose the database file").to_string());
        }
        None
    }
}

/// Список профилей в порядке названий.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Profiles {
    items: Vec<DbProfile>,
}

pub fn default_path() -> PathBuf {
    glib::user_config_dir()
        .join("sshdeck")
        .join("databases.ini")
}

impl Profiles {
    /// Отсутствующий файл — пустой список, а не ошибка.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(data) => Self::parse(&data).map_err(|err| format!("{}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(format!("{}: {err}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        }
        std::fs::write(path, self.serialize()).map_err(|err| format!("{}: {err}", path.display()))
    }

    pub fn parse(data: &str) -> Result<Self, String> {
        let key_file = glib::KeyFile::new();
        key_file
            .load_from_data(data, glib::KeyFileFlags::NONE)
            .map_err(|err| err.to_string())?;
        let mut items = Vec::new();
        for group in key_file.groups().iter() {
            let group = group.as_str();
            if !group.starts_with(GROUP_PREFIX) {
                continue;
            }
            let field = |key: &str| {
                key_file
                    .string(group, key)
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            };
            let Some(kind) = DbKind::from_id(field(KEY_KIND).trim()) else {
                continue;
            };
            let profile = DbProfile {
                name: field(KEY_NAME),
                kind,
                host: field(KEY_HOST),
                port: field(KEY_PORT).trim().parse().unwrap_or(0),
                user: field(KEY_USER),
                database: field(KEY_DATABASE),
                path: field(KEY_PATH),
                ssh: field(KEY_SSH),
                tls: key_file.boolean(group, KEY_TLS).unwrap_or(false),
                driver: field(KEY_DRIVER),
            }
            .normalized();
            if profile.kind == DbKind::Extension && profile.driver.is_empty() {
                continue;
            }
            items.push(profile);
        }
        let mut profiles = Self { items };
        profiles.sort();
        Ok(profiles)
    }

    fn serialize(&self) -> String {
        let key_file = glib::KeyFile::new();
        for (index, profile) in self.items.iter().enumerate() {
            let group = format!("{GROUP_PREFIX}-{}", index + 1);
            key_file.set_string(&group, KEY_NAME, &profile.name);
            key_file.set_string(&group, KEY_KIND, profile.kind.id());
            if profile.kind == DbKind::Extension {
                key_file.set_string(&group, KEY_DRIVER, &profile.driver);
            }
            // Неустановленное расширение: сохраняется всё, что было.
            let networked = profile.is_networked();
            if networked || profile.kind == DbKind::Extension {
                key_file.set_string(&group, KEY_HOST, &profile.host);
                key_file.set_string(&group, KEY_PORT, &profile.port.to_string());
                key_file.set_string(&group, KEY_USER, &profile.user);
                key_file.set_string(&group, KEY_DATABASE, &profile.database);
                if !profile.ssh.is_empty() {
                    key_file.set_string(&group, KEY_SSH, &profile.ssh);
                }
                key_file.set_boolean(&group, KEY_TLS, profile.tls);
            }
            if !networked || (profile.kind == DbKind::Extension && !profile.path.is_empty()) {
                key_file.set_string(&group, KEY_PATH, &profile.path);
            }
        }
        key_file.to_data().to_string()
    }

    fn sort(&mut self) {
        self.items
            .sort_by_cached_key(|profile| profile.name.to_lowercase());
    }

    pub fn items(&self) -> &[DbProfile] {
        &self.items
    }

    /// Добавляет профиль или заменяет `original`.
    pub fn upsert(&mut self, profile: DbProfile, original: Option<&DbProfile>) {
        let position = original.and_then(|original| self.items.iter().position(|p| p == original));
        match position {
            Some(position) => self.items[position] = profile,
            None => self.items.push(profile),
        }
        self.sort();
    }

    pub fn remove(&mut self, profile: &DbProfile) {
        self.items.retain(|candidate| candidate != profile);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg() -> DbProfile {
        DbProfile {
            name: "Прод".to_string(),
            host: "localhost".to_string(),
            user: "app".to_string(),
            database: "shop".to_string(),
            ssh: "root@10.0.0.5 -p 2222".to_string(),
            tls: true,
            ..DbProfile::template(DbKind::Postgres)
        }
    }

    #[test]
    fn round_trip() {
        let mut profiles = Profiles::default();
        profiles.upsert(pg(), None);
        profiles.upsert(
            DbProfile {
                name: "Локальная".to_string(),
                path: "/home/me/app.db".to_string(),
                ..DbProfile::template(DbKind::Sqlite)
            },
            None,
        );
        profiles.upsert(
            DbProfile {
                name: "maria".to_string(),
                port: 3307,
                ..DbProfile::template(DbKind::MySql)
            },
            None,
        );
        let parsed = Profiles::parse(&profiles.serialize()).expect("parse");
        assert_eq!(parsed, profiles);
        let names: Vec<_> = parsed.items().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["maria", "Локальная", "Прод"]);
    }

    #[test]
    fn unknown_kinds_and_other_groups_are_skipped() {
        let parsed =
            Profiles::parse("[database-1]\nkind=oracle\nname=x\n[other]\nkind=postgres\n[database-2]\nkind=extension\n").unwrap();
        assert!(parsed.items().is_empty());
    }

    #[test]
    fn extension_profiles_survive_without_extension() {
        let profile = DbProfile {
            name: "Аналитика".to_string(),
            host: "ch".to_string(),
            port: 8123,
            user: "default".to_string(),
            ..DbProfile::template_for(DbKind::Extension, "nonexistent-test-driver".to_string())
        };
        let mut profiles = Profiles::default();
        profiles.upsert(profile.clone(), None);
        let parsed = Profiles::parse(&profiles.serialize()).expect("parse");
        assert_eq!(parsed.items(), std::slice::from_ref(&profile));
        assert_eq!(profile.badge(), "NO");
        assert!(
            profile
                .problem()
                .is_some_and(|p| p.contains("is not installed"))
        );
    }

    #[test]
    fn missing_port_and_name_get_defaults() {
        let parsed =
            Profiles::parse("[database-1]\nkind=mysql\nhost=db\nuser=root\n").expect("parse");
        let profile = &parsed.items()[0];
        assert_eq!(profile.port, 3306);
        assert_eq!(profile.name, "root@db");
    }

    #[test]
    fn summaries() {
        assert_eq!(pg().summary(), "app@localhost/shop");
        let mut other = pg();
        other.port = 6432;
        other.database.clear();
        assert_eq!(other.summary(), "app@localhost:6432");
        let lite = DbProfile {
            path: "/data/x.sqlite".to_string(),
            ..DbProfile::template(DbKind::Sqlite)
        };
        assert_eq!(lite.summary(), "x.sqlite");
    }

    #[test]
    fn problems() {
        assert_eq!(pg().problem(), None);
        let mut bad = pg();
        bad.ssh = "-oProxyCommand=x".to_string();
        assert!(bad.problem().is_some());
        bad = pg();
        bad.host = " ".to_string();
        assert!(bad.problem().is_some());
        assert!(DbProfile::template(DbKind::Sqlite).problem().is_some());
    }

    #[test]
    fn upsert_replaces_original() {
        let mut profiles = Profiles::default();
        profiles.upsert(pg(), None);
        let mut changed = pg();
        changed.database = "other".to_string();
        profiles.upsert(changed.clone(), Some(&pg()));
        assert_eq!(profiles.items(), [changed.clone()]);
        profiles.remove(&changed);
        assert!(profiles.items().is_empty());
    }
}
