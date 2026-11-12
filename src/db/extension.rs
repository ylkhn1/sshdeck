//! Расширения: драйверы СУБД отдельными программами.
//!
//! Расширение — исполняемый файл на любом языке. Программа находит их сама
//! (ничего регистрировать не нужно):
//!
//! * в `~/.local/share/sshdeck/drivers/` и `/usr/share/sshdeck/drivers/`
//!   (любой исполняемый файл или каталог с исполняемым файлом `driver`);
//! * в `$PATH` под именем `sshdeck-driver-<что угодно>`.
//!
//! `<программа> --sshdeck-info` печатает описание драйвера одной строкой
//! JSON. Для подключения программа запускается без аргументов и общается
//! строками JSON через stdin/stdout: запрос `{"id":1,"method":"…",
//! "params":{…}}`, ответ `{"id":1,"result":…}` или `{"id":1,"error":"…"}`.
//! Протокол целиком описан в `extensions/README.md`.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

/// Версия протокола, которую понимает программа.
pub const PROTOCOL: u64 = 1;
/// Префикс расширений в `$PATH`.
const PATH_PREFIX: &str = "sshdeck-driver-";
const INFO_TIMEOUT: Duration = Duration::from_secs(5);
const STDERR_TAIL: usize = 4096;

/// Описание установленного расширения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionInfo {
    /// Идентификатор, под которым расширение записывается в профиль.
    pub id: String,
    /// Название СУБД для людей.
    pub name: String,
    /// Две-три буквы для значка.
    pub badge: String,
    pub default_port: u16,
    pub default_user: String,
    /// База — файл (как SQLite), а не сервер.
    pub file: bool,
    /// Исполняемый файл.
    pub path: PathBuf,
}

/// Каталог для расширений пользователя.
pub fn drivers_dir() -> PathBuf {
    gtk::glib::user_data_dir().join("sshdeck").join("drivers")
}

fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![drivers_dir()];
    for dir in gtk::glib::system_data_dirs() {
        dirs.push(dir.join("sshdeck").join("drivers"));
    }
    dirs.push(PathBuf::from("/usr/lib/sshdeck/drivers"));
    dirs
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Исполняемые файлы-кандидаты в порядке приоритета.
fn candidates() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in search_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                !path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'))
            })
            .collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                let driver = path.join("driver");
                if is_executable(&driver) {
                    found.push(driver);
                }
            } else if is_executable(&path) {
                found.push(path);
            }
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with(PATH_PREFIX))
                })
                .filter(|path| is_executable(path))
                .collect();
            paths.sort();
            found.extend(paths);
        }
    }
    found
}

/// Разбирает ответ `--sshdeck-info`.
fn parse_info(text: &str, path: &Path) -> Result<ExtensionInfo, String> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .ok_or("нет описания JSON")?;
    let value: Value = serde_json::from_str(line).map_err(|err| err.to_string())?;
    let protocol = value
        .get("sshdeck_driver")
        .and_then(Value::as_u64)
        .ok_or("нет поля sshdeck_driver")?;
    if protocol != PROTOCOL {
        return Err(format!("версия протокола {protocol}, нужна {PROTOCOL}"));
    }
    let text_field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let id = text_field("id");
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("id должен состоять из латинских букв, цифр, - и _".to_string());
    }
    let name = match text_field("name") {
        name if name.is_empty() => id.clone(),
        name => name,
    };
    let badge = match text_field("badge") {
        badge if badge.is_empty() => id.chars().take(2).collect::<String>().to_uppercase(),
        badge => badge.chars().take(3).collect(),
    };
    Ok(ExtensionInfo {
        id,
        name,
        badge,
        default_port: value
            .get("default_port")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .unwrap_or(0),
        default_user: text_field("default_user"),
        file: value.get("file").and_then(Value::as_bool).unwrap_or(false),
        path: path.to_path_buf(),
    })
}

/// Разбор описания (для тестов в других модулях).
#[cfg(test)]
pub fn scan_info_for_test(text: &str, path: &Path) -> ExtensionInfo {
    parse_info(text, path).expect("описание расширения")
}

/// Запускает `--sshdeck-info` с ограничением по времени.
fn query_info(path: &Path) -> Result<ExtensionInfo, String> {
    let mut child = std::process::Command::new(path)
        .arg("--sshdeck-info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| err.to_string())?;
    let mut stdout = child.stdout.take().ok_or("нет вывода")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.by_ref().take(64 * 1024).read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + INFO_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("не ответило за 5 с".to_string());
            }
        }
    }
    let text = reader.join().unwrap_or_default();
    parse_info(&text, path)
}

type CacheKey = (Option<SystemTime>, u64);
type Cache = HashMap<PathBuf, (CacheKey, Option<ExtensionInfo>)>;

/// Ответы `--sshdeck-info` по файлам: неизменённый файл второй раз не
/// запускается.
fn cache() -> &'static Mutex<Cache> {
    static CACHE: std::sync::OnceLock<Mutex<Cache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn registry() -> &'static RwLock<Vec<ExtensionInfo>> {
    static REGISTRY: RwLock<Vec<ExtensionInfo>> = RwLock::new(Vec::new());
    &REGISTRY
}

/// Ищет расширения заново и запоминает список. Новые и изменённые файлы
/// опрашиваются, остальные берутся из кеша, так что повторный вызов почти
/// ничего не стоит.
pub fn scan() -> Vec<ExtensionInfo> {
    let mut found: Vec<ExtensionInfo> = Vec::new();
    for path in candidates() {
        let key: CacheKey = std::fs::metadata(&path)
            .map(|meta| (meta.modified().ok(), meta.len()))
            .unwrap_or((None, 0));
        let cached = cache()
            .lock()
            .ok()
            .and_then(|cache| cache.get(&path).cloned())
            .filter(|(old, _)| *old == key)
            .map(|(_, info)| info);
        let info = match cached {
            Some(info) => info,
            None => {
                let info = query_info(&path).ok();
                if let Ok(mut cache) = cache().lock() {
                    cache.insert(path.clone(), (key, info.clone()));
                }
                info
            }
        };
        if let Some(info) = info
            && !found.iter().any(|known| known.id == info.id)
        {
            found.push(info);
        }
    }
    found.sort_by_cached_key(|info| info.name.to_lowercase());
    if let Ok(mut registry) = registry().write() {
        registry.clone_from(&found);
    }
    found
}

/// Расширения по последнему поиску.
pub fn installed() -> Vec<ExtensionInfo> {
    registry().read().map(|r| r.clone()).unwrap_or_default()
}

/// Добавляет расширение в список, минуя поиск (для тестов).
#[cfg(test)]
pub fn register(info: ExtensionInfo) {
    if let Ok(mut registry) = registry().write() {
        registry.retain(|known| known.id != info.id);
        registry.push(info);
    }
}

pub fn find(id: &str) -> Option<ExtensionInfo> {
    registry()
        .read()
        .ok()
        .and_then(|registry| registry.iter().find(|info| info.id == id).cloned())
}

// ------------------------------------------------------------ работающий драйвер

type Waiter = oneshot::Sender<Result<Value, String>>;

#[derive(Default)]
struct Pending {
    waiters: HashMap<u64, Waiter>,
    /// Программа завершилась; причина для следующих запросов.
    closed: Option<String>,
}

/// Запущенное расширение. Запросы можно отправлять одновременно: ответы
/// сопоставляются по `id`.
pub struct Plugin {
    name: String,
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Arc<Mutex<Pending>>,
    next: AtomicU64,
    _child: Child,
}

/// Ошибка вызова: текст и признак, что программа завершилась.
pub struct CallError {
    pub message: String,
    pub lost: bool,
}

impl Plugin {
    pub fn start(info: &ExtensionInfo) -> Result<Self, String> {
        let mut child = Command::new(&info.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| {
                format!(
                    "Не удалось запустить расширение {}: {err}",
                    info.path.display()
                )
            })?;
        let stdin = child.stdin.take().ok_or("нет stdin")?;
        let stdout = child.stdout.take().ok_or("нет stdout")?;
        let stderr = child.stderr.take().ok_or("нет stderr")?;
        let pending = Arc::new(Mutex::new(Pending::default()));
        let tail = Arc::new(Mutex::new(String::new()));

        // Последние строки stderr — для сообщения, если программа упадёт.
        {
            let tail = Arc::clone(&tail);
            tokio::spawn(async move {
                let mut stderr = stderr;
                let mut buf = [0u8; 1024];
                while let Ok(n) = stderr.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut tail) = tail.lock() {
                        tail.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if tail.len() > STDERR_TAIL {
                            let mut cut = tail.len() - STDERR_TAIL;
                            while !tail.is_char_boundary(cut) {
                                cut += 1;
                            }
                            tail.drain(..cut);
                        }
                    }
                }
            });
        }
        {
            let pending = Arc::clone(&pending);
            let name = info.name.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(message) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Some(id) = message.get("id").and_then(Value::as_u64) else {
                        continue;
                    };
                    let reply = match message.get("error") {
                        Some(error) if !error.is_null() => Err(match error {
                            Value::String(text) => text.clone(),
                            Value::Object(map) => map
                                .get("message")
                                .and_then(Value::as_str)
                                .map_or_else(|| error.to_string(), str::to_string),
                            other => other.to_string(),
                        }),
                        _ => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let waiter = pending
                        .lock()
                        .ok()
                        .and_then(|mut pending| pending.waiters.remove(&id));
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(reply);
                    }
                }
                // Вывод закрыт — программа завершилась (или закрыла stdout).
                tokio::time::sleep(Duration::from_millis(100)).await;
                let stderr = tail
                    .lock()
                    .map(|t| t.trim().to_string())
                    .unwrap_or_default();
                let reason = if stderr.is_empty() {
                    format!("Расширение {name} завершилось")
                } else {
                    format!("Расширение {name} завершилось:\n{stderr}")
                };
                if let Ok(mut pending) = pending.lock() {
                    for (_, waiter) in pending.waiters.drain() {
                        let _ = waiter.send(Err(reason.clone()));
                    }
                    pending.closed = Some(reason);
                }
            });
        }
        Ok(Self {
            name: info.name.clone(),
            stdin: tokio::sync::Mutex::new(stdin),
            pending,
            next: AtomicU64::new(1),
            _child: child,
        })
    }

    fn closed(&self) -> Option<String> {
        self.pending.lock().ok().and_then(|p| p.closed.clone())
    }

    async fn write_line(&self, message: &Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|err| err.to_string())?;
        stdin.flush().await.map_err(|err| err.to_string())
    }

    /// Вызывает метод и ждёт ответа.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        if let Some(reason) = self.closed() {
            return Err(CallError {
                message: reason,
                lost: true,
            });
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.waiters.insert(id, tx);
        }
        let request = json!({"id": id, "method": method, "params": params});
        if let Err(err) = self.write_line(&request).await {
            if let Ok(mut pending) = self.pending.lock() {
                pending.waiters.remove(&id);
            }
            return Err(CallError {
                message: self.closed().unwrap_or_else(|| {
                    format!("Расширение {} не принимает запросы: {err}", self.name)
                }),
                lost: true,
            });
        }
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(CallError {
                lost: self.closed().is_some(),
                message,
            }),
            Err(_) => Err(CallError {
                message: self
                    .closed()
                    .unwrap_or_else(|| format!("Расширение {} не ответило", self.name)),
                lost: true,
            }),
        }
    }

    /// Просьба прервать текущий запрос; ответа на неё не бывает.
    pub async fn cancel(&self) {
        let _ = self.write_line(&json!({"method": "cancel"})).await;
    }
}

/// Значение из JSON как текст ячейки.
pub fn json_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Number(number) => Some(number.to_string()),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_parsing() {
        let path = Path::new("/x/driver");
        let info = parse_info(
            "debug line\n{\"sshdeck_driver\":1,\"id\":\"clickhouse\",\"name\":\"ClickHouse\",\
             \"default_port\":8123,\"default_user\":\"default\"}\n",
            path,
        )
        .expect("info");
        assert_eq!(info.id, "clickhouse");
        assert_eq!(info.badge, "CL");
        assert_eq!(info.default_port, 8123);
        assert!(!info.file);
        assert!(parse_info("{\"sshdeck_driver\":2,\"id\":\"x\"}", path).is_err());
        assert!(parse_info("{\"sshdeck_driver\":1,\"id\":\"a b\"}", path).is_err());
        assert!(parse_info("nothing", path).is_err());
    }

    /// Протокол на настоящем процессе: ответы по `id`, ошибки, падение.
    #[test]
    fn plugin_protocol() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fake");
        std::fs::write(
            &path,
            r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    if msg["method"] == "echo":
        print(json.dumps({"id": msg["id"], "result": msg["params"]}), flush=True)
    elif msg["method"] == "fail":
        print(json.dumps({"id": msg["id"], "error": "плохой запрос"}), flush=True)
    else:
        sys.stderr.write("boom\n")
        sys.exit(3)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let info = ExtensionInfo {
            id: "fake".into(),
            name: "Fake".into(),
            badge: "FK".into(),
            default_port: 0,
            default_user: String::new(),
            file: false,
            path,
        };
        let result = crate::db::spawn(async move {
            let plugin = Plugin::start(&info).expect("start");
            let (a, b) = tokio::join!(
                plugin.call("echo", json!({"n": 1})),
                plugin.call("echo", json!({"n": 2}))
            );
            assert_eq!(a.ok(), Some(json!({"n": 1})));
            assert_eq!(b.ok(), Some(json!({"n": 2})));
            let error = plugin.call("fail", json!({})).await.expect_err("error");
            assert_eq!(error.message, "плохой запрос");
            assert!(!error.lost);
            let crash = plugin.call("die", json!({})).await.expect_err("crash");
            assert!(crash.lost);
            assert!(crash.message.contains("boom"), "{}", crash.message);
            let after = plugin.call("echo", json!({})).await.expect_err("closed");
            assert!(after.lost);
        })
        .recv_blocking();
        assert!(result.is_ok(), "проверка упала");
    }

    #[test]
    fn json_values_become_text() {
        assert_eq!(json_text(&json!(null)), None);
        assert_eq!(json_text(&json!("a")), Some("a".to_string()));
        assert_eq!(json_text(&json!(1.5)), Some("1.5".to_string()));
        assert_eq!(json_text(&json!(true)), Some("true".to_string()));
        assert_eq!(json_text(&json!([1])), Some("[1]".to_string()));
    }
}
