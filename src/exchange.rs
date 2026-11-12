//! Импорт и экспорт сохранённых подключений.
//!
//! Экспорт — JSON-файл, повторяющий дерево из боковой панели: папки с
//! цветами и подключения с командой после входа и пробросами портов.
//! Паролей в нём нет: sshdeck их не хранит.
//!
//! Импорт принимает такой файл или ssh_config (например, с другой машины)
//! и добавляет записи к уже сохранённым. Подключение с тем же названием и
//! адресом в той же папке пропускается, поэтому повторный импорт ничего не
//! задваивает.

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::connections::{Connection, Connections, FolderPath, Node, clean};
use crate::forward::Forward;
use crate::ssh_config::{self, HostEntry};

/// Значение поля `format`: по нему файл узнаётся при импорте.
const FORMAT: &str = "sshdeck-connections";
/// Версия формата. Файл более новой версии не читается.
const VERSION: u64 = 1;
/// Имя файла, которое предлагается при экспорте.
pub const DEFAULT_FILE_NAME: &str = "sshdeck-connections.json";

/// Итог импорта.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Сколько подключений добавлено.
    pub added: usize,
    /// Сколько уже было в списке и пропущено.
    pub skipped: usize,
    /// Файл оказался ssh-конфигом: из него взяты только адреса.
    pub from_ssh_config: bool,
}

/// Всё дерево подключений в виде JSON.
pub fn export(list: &Connections) -> String {
    let document = json!({
        "format": FORMAT,
        "version": VERSION,
        "items": export_nodes(list.nodes()),
    });
    // Сериализация `Value` ошибиться не может.
    serde_json::to_string_pretty(&document).unwrap_or_default() + "\n"
}

fn export_nodes(nodes: &[Node]) -> Vec<Value> {
    nodes
        .iter()
        .map(|node| match node {
            Node::Folder(folder) => {
                let mut entry = Map::new();
                entry.insert("folder".into(), json!(folder.name));
                if let Some(color) = &folder.color {
                    entry.insert("color".into(), json!(color));
                }
                entry.insert("items".into(), Value::Array(export_nodes(&folder.children)));
                Value::Object(entry)
            }
            Node::Connection(connection) => {
                let mut entry = Map::new();
                entry.insert("name".into(), json!(connection.name));
                entry.insert("address".into(), json!(connection.address));
                if !connection.startup.is_empty() {
                    entry.insert("startup".into(), json!(connection.startup));
                }
                if !connection.forwards.is_empty() {
                    let forwards: Vec<String> =
                        connection.forwards.iter().map(ToString::to_string).collect();
                    entry.insert("forwards".into(), json!(forwards));
                }
                Value::Object(entry)
            }
        })
        .collect()
}

/// Добавляет к `list` подключения из файла `path` — экспорта sshdeck или
/// ssh_config. `home` нужен для `Include` в ssh_config. Возвращает новый
/// список: при ошибке исходный остаётся нетронутым.
pub fn import(list: &Connections, path: &Path, home: &Path) -> Result<(Connections, Report), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| tr!("Could not read {}: {}", path.display(), err))?;
    if text.trim_start().starts_with('{') {
        return import_json(list, &text);
    }
    let parsed = ssh_config::load(path, home);
    if parsed.hosts.is_empty() {
        return Err(tr!(
            "The file is neither an sshdeck export nor an ssh config with Host entries"
        )
        .to_string());
    }
    Ok(import_hosts(list, &parsed.hosts))
}

/// Импорт экспорта sshdeck.
pub fn import_json(list: &Connections, text: &str) -> Result<(Connections, Report), String> {
    let document: Value = serde_json::from_str(text)
        .map_err(|err| tr!("The file is not valid JSON: {}", err))?;
    let not_export = || tr!("This JSON file is not an sshdeck connections export").to_string();
    if document.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(not_export());
    }
    match document.get("version").and_then(Value::as_u64) {
        Some(version) if version > VERSION => {
            return Err(tr!(
                "The file was made by a newer version of sshdeck; update the app to import it"
            )
            .to_string());
        }
        Some(_) => {}
        None => return Err(not_export()),
    }
    let items = document
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(not_export)?;
    let mut result = list.clone();
    let mut report = Report::default();
    import_items(&mut result, items, &mut Vec::new(), &mut report)?;
    Ok((result, report))
}

fn import_items(
    list: &mut Connections,
    items: &[Value],
    path: &mut FolderPath,
    report: &mut Report,
) -> Result<(), String> {
    for item in items {
        let entry = item
            .as_object()
            .ok_or_else(|| tr!("The file has an entry that is not an object").to_string())?;
        let text = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or("");

        if entry.contains_key("folder") {
            let name = clean(text("folder"));
            if name.is_empty() {
                return Err(tr!("The file has a folder without a name").to_string());
            }
            path.push(name);
            let loc = list.ensure_folder(path);
            // Цвет уже существующей папки не перекрашивается.
            if list.folder_color(&loc).is_none() && !text("color").is_empty() {
                list.set_folder_color(&loc, Some(text("color")));
            }
            if let Some(children) = entry.get("items").and_then(Value::as_array) {
                import_items(list, children, path, report)?;
            }
            path.pop();
            continue;
        }

        let address = text("address");
        if clean(address).is_empty() {
            return Err(tr!("The file has a connection without an address").to_string());
        }
        let mut forwards = Vec::new();
        for spec in entry
            .get("forwards")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            forwards.push(Forward::parse(spec.as_str().unwrap_or_default())?);
        }
        let connection = Connection::new(text("name"), address)
            .with_startup(text("startup"))
            .with_forwards(forwards);
        add_unique(list, connection, path, report);
    }
    Ok(())
}

/// Хосты ssh_config как подключения в корне списка: адрес собирается из
/// `User`, `HostName` и `Port`.
pub fn import_hosts(list: &Connections, hosts: &[HostEntry]) -> (Connections, Report) {
    let mut result = list.clone();
    let mut report = Report {
        from_ssh_config: true,
        ..Report::default()
    };
    for host in hosts {
        let mut address = host.hostname.clone().unwrap_or_else(|| host.alias.clone());
        if let Some(user) = &host.user {
            address = format!("{user}@{address}");
        }
        if let Some(port) = host.port.as_deref().filter(|port| *port != "22") {
            address.push_str(&format!(" -p {port}"));
        }
        add_unique(&mut result, Connection::new(&host.alias, &address), &[], &mut report);
    }
    (result, report)
}

/// Добавляет подключение, если в папке нет такого же названия с тем же
/// адресом.
fn add_unique(list: &mut Connections, connection: Connection, folder: &[String], report: &mut Report) {
    let duplicate = list.walk().iter().any(|item| {
        item.parent == folder
            && matches!(item.node, Node::Connection(existing)
                if existing.name == connection.name && existing.address == connection.address)
    });
    if duplicate {
        report.skipped += 1;
    } else {
        list.add(connection, folder);
        report.added += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{Report, export, import_hosts, import_json};
    use crate::connections::{Connection, Connections, Node};
    use crate::forward::Forward;
    use crate::ssh_config::HostEntry;

    fn path(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    /// Подключение в корне, вложенная папка с цветом, пробросы, команда
    /// после входа и пустая папка.
    fn sample() -> Connections {
        let mut list = Connections::default();
        list.add(Connection::new("dev-box", "dev@10.0.0.5 -p 2222"), &[]);
        let forwards = vec![
            Forward::parse("-L 8080:localhost:80").unwrap(),
            Forward::parse("-D 1080").unwrap(),
        ];
        list.add(
            Connection::new("web-prod", "deploy@web-prod")
                .with_startup("cd ~/app")
                .with_forwards(forwards),
            &path(&["Production", "Web"]),
        );
        let loc = list.folder_loc(&path(&["Production"])).unwrap();
        list.set_folder_color(&loc, Some("red"));
        list.ensure_folder(&path(&["Empty"]));
        list
    }

    #[test]
    fn export_then_import_into_empty_list_restores_the_tree() {
        let original = sample();
        let json = export(&original);
        assert!(json.contains("\"format\": \"sshdeck-connections\""));
        assert!(json.contains("\"-L 8080:localhost:80\""));
        assert!(json.contains("\"color\": \"red\""));
        let (imported, report) = import_json(&Connections::default(), &json).unwrap();
        assert_eq!(imported, original);
        assert_eq!(
            report,
            Report {
                added: 2,
                skipped: 0,
                from_ssh_config: false
            }
        );
    }

    #[test]
    fn importing_twice_adds_nothing_new() {
        let original = sample();
        let json = export(&original);
        let (again, report) = import_json(&original, &json).unwrap();
        assert_eq!(again, original);
        assert_eq!((report.added, report.skipped), (0, 2));
    }

    #[test]
    fn existing_folder_keeps_its_color() {
        let mut list = Connections::default();
        let loc = list.ensure_folder(&path(&["Production"]));
        list.set_folder_color(&loc, Some("blue"));
        let (merged, _) = import_json(&list, &export(&sample())).unwrap();
        let loc = merged.folder_loc(&path(&["Production"])).unwrap();
        assert_eq!(merged.folder_color(&loc), Some("blue"));
    }

    #[test]
    fn rejects_foreign_and_newer_files() {
        let empty = Connections::default();
        assert!(import_json(&empty, "{\"hosts\": []}").is_err());
        assert!(import_json(&empty, "not json").is_err());
        let newer = "{\"format\": \"sshdeck-connections\", \"version\": 99, \"items\": []}";
        assert!(import_json(&empty, newer).unwrap_err().contains("newer"));
    }

    #[test]
    fn bad_entry_fails_the_whole_import() {
        let broken = "{\"format\": \"sshdeck-connections\", \"version\": 1, \"items\": [\
            {\"name\": \"ok\", \"address\": \"a@b\"},\
            {\"name\": \"bad\", \"address\": \"c@d\", \"forwards\": [\"-X 1\"]}]}";
        assert!(import_json(&sample(), broken).is_err());
        let no_address = "{\"format\": \"sshdeck-connections\", \"version\": 1, \
            \"items\": [{\"name\": \"x\"}]}";
        assert!(import_json(&sample(), no_address).is_err());
    }

    #[test]
    fn import_detects_file_kind() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::write(
            &config,
            "Host nas\n  HostName 192.168.1.20\n  User admin\n  Port 2299\n\nHost *\n  ServerAliveInterval 30\n",
        )
        .unwrap();
        let (list, report) = super::import(&Connections::default(), &config, dir.path()).unwrap();
        assert!(report.from_ssh_config);
        assert_eq!(report.added, 1);
        assert_eq!(list.walk().len(), 1);

        let json = dir.path().join("export.json");
        std::fs::write(&json, export(&sample())).unwrap();
        let (list, report) = super::import(&Connections::default(), &json, dir.path()).unwrap();
        assert!(!report.from_ssh_config);
        assert_eq!(list, sample());

        let junk = dir.path().join("notes.txt");
        std::fs::write(&junk, "just some text\n").unwrap();
        assert!(super::import(&Connections::default(), &junk, dir.path()).is_err());
        assert!(super::import(&Connections::default(), &dir.path().join("absent"), dir.path()).is_err());
    }

    #[test]
    fn ssh_config_hosts_become_connections() {
        let hosts = [
            HostEntry {
                alias: "nas".into(),
                hostname: Some("192.168.1.20".into()),
                user: Some("admin".into()),
                port: Some("2299".into()),
            },
            HostEntry {
                alias: "pi".into(),
                hostname: Some("raspberry.local".into()),
                user: None,
                port: Some("22".into()),
            },
        ];
        let (list, report) = import_hosts(&Connections::default(), &hosts);
        assert!(report.from_ssh_config);
        assert_eq!(report.added, 2);
        let addresses: Vec<String> = list
            .walk()
            .iter()
            .filter_map(|item| match item.node {
                Node::Connection(c) => Some(format!("{}={}", c.name, c.address)),
                Node::Folder(_) => None,
            })
            .collect();
        assert_eq!(addresses, ["nas=admin@192.168.1.20 -p 2299", "pi=raspberry.local"]);
    }
}
