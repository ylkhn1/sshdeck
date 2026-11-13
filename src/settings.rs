//! Настройки приложения: `~/.config/sshdeck/settings.ini`.
//!
//! Здесь хранятся шрифт и тема терминала, оформление окна, сохранённые
//! команды и то, какие папки боковой панели свёрнуты. Сохранённые
//! подключения лежат отдельно (`crate::connections`); паролей и истории
//! приложение не хранит.
//!
//! Свёрнутые папки — группа `[sidebar]`, ключ `collapsed`: список GLib
//! (через `;`) полных путей папок, записанных как в `connections.ini`
//! (`Работа/Сервера`, см. [`join_path`]). Пустой список не пишется.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gtk::glib;

use crate::connections::{FolderPath, join_path, split_path};

pub const DEFAULT_FONT: &str = "Monospace 10";
/// Тема терминала, которая следует за светлым или тёмным оформлением окна.
pub const DEFAULT_THEME: &str = "system";
/// Оформление окна: `system`, `light` или `dark`.
pub const DEFAULT_SCHEME: &str = "system";
/// Язык интерфейса: `system` (как в системе) или код из `i18n::LANGUAGES`.
pub const DEFAULT_LANGUAGE: &str = "system";

const GROUP_TERMINAL: &str = "terminal";
const KEY_FONT: &str = "font";
const KEY_THEME: &str = "theme";
const GROUP_APPEARANCE: &str = "appearance";
const KEY_SCHEME: &str = "scheme";
const KEY_FILES_PANEL: &str = "files_panel";
const KEY_LANGUAGE: &str = "language";
const GROUP_SNIPPET: &str = "snippet";
const KEY_NAME: &str = "name";
const KEY_COMMAND: &str = "command";
const GROUP_SIDEBAR: &str = "sidebar";
const KEY_COLLAPSED: &str = "collapsed";

/// Сохранённая команда, которую можно отправить в терминал одним щелчком.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub name: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Описание шрифта в формате Pango, например `JetBrains Mono 12`.
    pub terminal_font: String,
    /// Идентификатор темы терминала (см. `ui::terminal::THEMES`).
    pub terminal_theme: String,
    pub color_scheme: String,
    /// Видна ли панель файлов во вкладках.
    pub show_files: bool,
    /// Язык интерфейса; применяется при следующем запуске.
    pub language: String,
    pub snippets: Vec<Snippet>,
    /// Пути свёрнутых папок боковой панели (по порядку — чтобы файл не
    /// менялся от запуска к запуску).
    pub collapsed_folders: BTreeSet<FolderPath>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            terminal_font: DEFAULT_FONT.to_string(),
            terminal_theme: DEFAULT_THEME.to_string(),
            color_scheme: DEFAULT_SCHEME.to_string(),
            show_files: true,
            language: DEFAULT_LANGUAGE.to_string(),
            snippets: Vec::new(),
            collapsed_folders: BTreeSet::new(),
        }
    }
}

fn settings_path() -> PathBuf {
    glib::user_config_dir().join("sshdeck").join("settings.ini")
}

impl Settings {
    /// Читает настройки; отсутствующий или повреждённый файл даёт значения
    /// по умолчанию.
    pub fn load() -> Self {
        std::fs::read_to_string(settings_path())
            .map(|data| Self::parse(&data))
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&settings_path())
    }

    fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        }
        std::fs::write(path, self.serialize()).map_err(|err| format!("{}: {err}", path.display()))
    }

    fn parse(data: &str) -> Self {
        let mut settings = Self::default();
        let key_file = glib::KeyFile::new();
        if key_file
            .load_from_data(data, glib::KeyFileFlags::NONE)
            .is_err()
        {
            return settings;
        }
        let field = |group: &str, key: &str| {
            key_file
                .string(group, key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        if let Some(font) = field(GROUP_TERMINAL, KEY_FONT) {
            settings.terminal_font = font;
        }
        if let Some(theme) = field(GROUP_TERMINAL, KEY_THEME) {
            settings.terminal_theme = theme;
        }
        if let Some(scheme) = field(GROUP_APPEARANCE, KEY_SCHEME) {
            settings.color_scheme = scheme;
        }
        if let Ok(show) = key_file.boolean(GROUP_APPEARANCE, KEY_FILES_PANEL) {
            settings.show_files = show;
        }
        if let Some(language) = field(GROUP_APPEARANCE, KEY_LANGUAGE) {
            settings.language = language;
        }
        if let Ok(list) = key_file.string_list(GROUP_SIDEBAR, KEY_COLLAPSED) {
            settings.collapsed_folders = list
                .iter()
                .map(|item| split_path(item.as_str()))
                .filter(|path| !path.is_empty())
                .collect();
        }
        for group in key_file.groups().iter() {
            let group = group.as_str();
            if !group.starts_with(GROUP_SNIPPET) {
                continue;
            }
            if let Some(command) = field(group, KEY_COMMAND) {
                let name = field(group, KEY_NAME).unwrap_or_else(|| command.clone());
                settings.snippets.push(Snippet { name, command });
            }
        }
        settings
    }

    fn serialize(&self) -> String {
        let key_file = glib::KeyFile::new();
        key_file.set_string(GROUP_TERMINAL, KEY_FONT, &self.terminal_font);
        key_file.set_string(GROUP_TERMINAL, KEY_THEME, &self.terminal_theme);
        key_file.set_string(GROUP_APPEARANCE, KEY_SCHEME, &self.color_scheme);
        key_file.set_boolean(GROUP_APPEARANCE, KEY_FILES_PANEL, self.show_files);
        key_file.set_string(GROUP_APPEARANCE, KEY_LANGUAGE, &self.language);
        for (index, snippet) in self.snippets.iter().enumerate() {
            let group = format!("{GROUP_SNIPPET}-{}", index + 1);
            key_file.set_string(&group, KEY_NAME, &snippet.name);
            key_file.set_string(&group, KEY_COMMAND, &snippet.command);
        }
        if !self.collapsed_folders.is_empty() {
            let paths = self.collapsed_folders.iter().map(|path| join_path(path));
            key_file.set_value(GROUP_SIDEBAR, KEY_COLLAPSED, &list_value(paths));
        }
        key_file.to_data().to_string()
    }
}

/// Значение-список ключевого файла GLib: каждый элемент экранирован так же,
/// как это делает `g_key_file_set_string_list` (в привязках glib-rs её
/// нет), и заканчивается `;`. Читается обратно `KeyFile::string_list`.
fn list_value(items: impl IntoIterator<Item = String>) -> String {
    let mut value = String::new();
    for item in items {
        for (index, c) in item.chars().enumerate() {
            match c {
                ' ' if index == 0 => value.push_str("\\s"),
                '\n' => value.push_str("\\n"),
                '\t' => value.push_str("\\t"),
                '\r' => value.push_str("\\r"),
                '\\' => value.push_str("\\\\"),
                ';' => value.push_str("\\;"),
                _ => value.push(c),
            }
        }
        value.push(';');
    }
    value
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_FONT, DEFAULT_SCHEME, DEFAULT_THEME, Settings, Snippet};
    use crate::connections::FolderPath;

    fn folder(parts: &[&str]) -> FolderPath {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn defaults_for_empty_or_broken_file() {
        assert_eq!(Settings::parse(""), Settings::default());
        assert_eq!(Settings::parse("не ini"), Settings::default());
        let defaults = Settings::default();
        assert_eq!(defaults.terminal_font, DEFAULT_FONT);
        assert_eq!(defaults.terminal_theme, DEFAULT_THEME);
        assert_eq!(defaults.color_scheme, DEFAULT_SCHEME);
    }

    #[test]
    fn old_file_with_only_font_still_loads() {
        let settings = Settings::parse("[terminal]\nfont=JetBrains Mono 12\n");
        assert_eq!(settings.terminal_font, "JetBrains Mono 12");
        assert_eq!(settings.terminal_theme, DEFAULT_THEME);
        assert!(settings.show_files);
        assert_eq!(settings.language, super::DEFAULT_LANGUAGE);
        assert!(settings.snippets.is_empty());
        assert!(settings.collapsed_folders.is_empty());
    }

    #[test]
    fn round_trip() {
        let settings = Settings {
            terminal_font: "Fira Code 13".to_string(),
            terminal_theme: "dracula".to_string(),
            color_scheme: "dark".to_string(),
            show_files: false,
            language: "es".to_string(),
            snippets: vec![
                Snippet {
                    name: "Логи nginx".to_string(),
                    command: "tail -f /var/log/nginx/error.log | grep -v \"; ok\"".to_string(),
                },
                Snippet {
                    name: "Диски".to_string(),
                    command: "df -h".to_string(),
                },
            ],
            collapsed_folders: [folder(&["Работа"]), folder(&["Работа", "Сервера"])].into(),
        };
        assert_eq!(Settings::parse(&settings.serialize()), settings);
    }

    #[test]
    fn collapsed_folders_with_any_characters_survive_a_round_trip() {
        // Разделители пути и списка, экранирующие символы, синтаксис
        // ключевого файла (`=`, `#`, `[]`), кавычки, не-ASCII.
        let names = [
            "a/b",
            "back\\slash",
            "trailing\\",
            "\\/",
            "semi;colon",
            "\\;",
            ";",
            "x = y # z",
            "[group]",
            "\"quoted\" 'single'",
            "\\n not a newline",
            "Сервера 🚀",
        ];
        let mut settings = Settings::default();
        for name in names {
            settings.collapsed_folders.insert(folder(&[name]));
            settings
                .collapsed_folders
                .insert(folder(&["Корень", name, "Вложенная"]));
        }
        let data = settings.serialize();
        assert_eq!(Settings::parse(&data), settings, "{data}");
    }

    #[test]
    fn collapsed_folders_are_written_in_a_stable_readable_form() {
        let mut settings = Settings::default();
        settings
            .collapsed_folders
            .insert(folder(&["Работа", "Сервера"]));
        settings.collapsed_folders.insert(folder(&["A/B"]));
        let data = settings.serialize();
        assert!(
            data.contains("[sidebar]\ncollapsed=A\\\\/B;Работа/Сервера;\n"),
            "{data}"
        );
        // Без свёрнутых папок группы нет вовсе.
        assert!(!Settings::default().serialize().contains("sidebar"));
    }

    #[test]
    fn hand_edited_collapsed_list_is_cleaned() {
        let settings = Settings::parse("[sidebar]\ncollapsed= Работа / Сервера ;;/;EU\n");
        assert_eq!(
            settings.collapsed_folders,
            [folder(&["Работа", "Сервера"]), folder(&["EU"])].into()
        );
    }

    #[test]
    fn list_value_is_read_back_by_glib() {
        // Таких символов в путях папок не бывает (имена чистятся), но
        // экранирование должно совпадать с GLib и для них.
        let items = [
            " lead", "a\nb", "t\tr\r", "\\;", ";", "\\", "end\\", "x;y\\z",
        ];
        let value = super::list_value(items.iter().map(|item| item.to_string()));
        let key_file = gtk::glib::KeyFile::new();
        key_file.set_value("g", "k", &value);
        let data = key_file.to_data();
        let reread = gtk::glib::KeyFile::new();
        reread
            .load_from_data(&data, gtk::glib::KeyFileFlags::NONE)
            .expect("load");
        let list = reread.string_list("g", "k").expect("list");
        let list: Vec<&str> = list.iter().map(|item| item.as_str()).collect();
        assert_eq!(list, items);
    }

    #[test]
    fn save_creates_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a/b/settings.ini");
        let settings = Settings::default();
        settings.save_to(&path).expect("save");
        let data = std::fs::read_to_string(&path).expect("read");
        assert_eq!(Settings::parse(&data), settings);
    }
}
