//! Настройки приложения: `~/.config/sshdeck/settings.ini`.
//!
//! Здесь хранятся шрифт и тема терминала, оформление окна и сохранённые
//! команды. Сохранённые подключения лежат отдельно (`crate::connections`);
//! паролей и истории приложение не хранит.

use std::path::{Path, PathBuf};

use gtk::glib;

pub const DEFAULT_FONT: &str = "Monospace 10";
/// Тема терминала, которая следует за светлым или тёмным оформлением окна.
pub const DEFAULT_THEME: &str = "system";
/// Оформление окна: `system`, `light` или `dark`.
pub const DEFAULT_SCHEME: &str = "system";

const GROUP_TERMINAL: &str = "terminal";
const KEY_FONT: &str = "font";
const KEY_THEME: &str = "theme";
const GROUP_APPEARANCE: &str = "appearance";
const KEY_SCHEME: &str = "scheme";
const KEY_FILES_PANEL: &str = "files_panel";
const GROUP_SNIPPET: &str = "snippet";
const KEY_NAME: &str = "name";
const KEY_COMMAND: &str = "command";

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
    pub snippets: Vec<Snippet>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            terminal_font: DEFAULT_FONT.to_string(),
            terminal_theme: DEFAULT_THEME.to_string(),
            color_scheme: DEFAULT_SCHEME.to_string(),
            show_files: true,
            snippets: Vec::new(),
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
        for (index, snippet) in self.snippets.iter().enumerate() {
            let group = format!("{GROUP_SNIPPET}-{}", index + 1);
            key_file.set_string(&group, KEY_NAME, &snippet.name);
            key_file.set_string(&group, KEY_COMMAND, &snippet.command);
        }
        key_file.to_data().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_FONT, DEFAULT_SCHEME, DEFAULT_THEME, Settings, Snippet};

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
        assert!(settings.snippets.is_empty());
    }

    #[test]
    fn round_trip() {
        let settings = Settings {
            terminal_font: "Fira Code 13".to_string(),
            terminal_theme: "dracula".to_string(),
            color_scheme: "dark".to_string(),
            show_files: false,
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
        };
        assert_eq!(Settings::parse(&settings.serialize()), settings);
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
