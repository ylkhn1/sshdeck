//! Клиент баз данных: профили подключений, SSH-туннели и драйверы.
//!
//! Как и SFTP, работает в своём tokio-рантайме и ничего не знает о GTK:
//! интерфейс отправляет [`Request`] через [`DbWorker::send`] и получает
//! [`Event`] из канала.

mod driver;
#[cfg(test)]
mod live_tests;
pub mod profile;
pub mod secrets;
pub mod sql;
mod tunnel;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_channel::Sender;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use driver::{DbError, MAX_ROWS, QueryOutput, TableInfo};
pub use profile::{DbKind, DbProfile, Profiles};

use driver::Backend;

/// Что нужно для подключения.
pub struct ConnectRequest {
    pub profile: DbProfile,
    pub password: String,
    /// Сокет мастер-соединения открытой вкладки к SSH-серверу профиля.
    pub control: Option<PathBuf>,
}

#[derive(Debug)]
pub enum Request {
    /// Выполнить SQL; `id` возвращается в [`Event::QueryDone`].
    Query { id: u64, sql: String },
    /// Структура таблицы (колонки, типы); ответ — тоже `QueryDone`.
    Structure {
        id: u64,
        schema: String,
        table: String,
    },
    /// Перечитать список таблиц.
    Tables,
    /// Прервать выполняющийся запрос.
    Cancel,
}

#[derive(Debug)]
pub enum Event {
    Connected {
        /// Версия сервера, например «PostgreSQL 16.4».
        version: String,
        /// Как идёт соединение, для строки состояния.
        route: String,
    },
    ConnectFailed(String),
    Tables(Result<Vec<TableInfo>, DbError>),
    /// Базы на сервере (только PostgreSQL) и та, к которой подключены.
    Databases {
        names: Vec<String>,
        current: String,
    },
    QueryDone {
        id: u64,
        result: Result<QueryOutput, DbError>,
        elapsed: Duration,
    },
}

fn runtime() -> Result<&'static Runtime, String> {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("sshdeck-db")
        .enable_all()
        .build()
        .map_err(|err| format!("Не удалось запустить фоновые потоки: {err}"))?;
    Ok(RUNTIME.get_or_init(|| runtime))
}

/// Выполняет задачу в фоновом рантайме; результат придёт в канал (его
/// удобно ждать из главного потока GTK). Если рантайм не запустился,
/// канал закрывается без результата.
pub fn spawn<T: Send + 'static>(
    job: impl Future<Output = T> + Send + 'static,
) -> async_channel::Receiver<T> {
    let (tx, rx) = async_channel::bounded(1);
    if let Ok(runtime) = runtime() {
        runtime.spawn(async move {
            let _ = tx.send(job.await).await;
        });
    }
    rx
}

/// Похоже ли сообщение об ошибке подключения на неверный или пустой пароль.
pub fn is_auth_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "password authentication failed",
        "сервер требует пароль",
        "access denied",
        "(код 1045",
        "authentication failed",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Соединение одной вкладки с базой. При уничтожении закрывает соединение
/// и туннель.
pub struct DbWorker {
    tx: mpsc::UnboundedSender<Request>,
    stop: CancellationToken,
}

impl DbWorker {
    pub fn start(request: ConnectRequest, events: Sender<Event>) -> Result<Self, String> {
        let runtime = runtime()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        runtime.spawn(run(request, rx, events, stop.clone()));
        Ok(Self { tx, stop })
    }

    pub fn send(&self, request: Request) {
        let _ = self.tx.send(request);
    }
}

impl Drop for DbWorker {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Имя сокета туннеля: PostgreSQL ищет в каталоге-«хосте» файл
/// `.s.PGSQL.<порт>`, MySQL принимает путь к сокету целиком.
fn socket_name(profile: &DbProfile) -> String {
    match profile.kind {
        DbKind::Postgres => format!(".s.PGSQL.{}", profile.port),
        _ => "db.sock".to_string(),
    }
}

async fn connect(
    request: &ConnectRequest,
) -> Result<(Option<tunnel::Tunnel>, Backend, String, String), String> {
    let profile = &request.profile;
    if let Some(problem) = profile.problem() {
        return Err(problem);
    }
    let tunnel = if profile.kind.is_networked() && !profile.ssh.is_empty() {
        let target = crate::session::validate_target(&profile.ssh)?;
        let name = socket_name(profile);
        Some(
            tunnel::open(tunnel::Spec {
                target: &target,
                control: request.control.as_deref(),
                host: &profile.host,
                port: profile.port,
                socket_name: &name,
            })
            .await?,
        )
    } else {
        None
    };
    let (backend, version) = Backend::connect(profile, &request.password, tunnel.as_ref())
        .await
        .map_err(|err| err.message)?;
    let route = match &tunnel {
        Some(tunnel) if tunnel.via_tab() => format!("через вкладку {}", profile.ssh),
        Some(_) => format!("через SSH {}", profile.ssh),
        None if profile.kind.is_networked() => {
            if profile.tls {
                "напрямую, TLS".to_string()
            } else {
                "напрямую".to_string()
            }
        }
        None => profile.summary(),
    };
    Ok((tunnel, backend, version, route))
}

async fn run(
    request: ConnectRequest,
    mut rx: mpsc::UnboundedReceiver<Request>,
    events: Sender<Event>,
    stop: CancellationToken,
) {
    let connected = tokio::select! {
        _ = stop.cancelled() => return,
        result = connect(&request) => result,
    };
    let (tunnel, backend, version, route) = match connected {
        Ok(parts) => parts,
        Err(message) => {
            let _ = events.send(Event::ConnectFailed(message)).await;
            return;
        }
    };
    let backend = Arc::new(backend);
    let _ = events.send(Event::Connected { version, route }).await;
    spawn_tables(&backend, &events, &stop);
    spawn_databases(&backend, &events, &stop);

    loop {
        let request = tokio::select! {
            _ = stop.cancelled() => break,
            request = rx.recv() => match request {
                Some(request) => request,
                None => break,
            },
        };
        match request {
            Request::Query { id, sql } => {
                let backend = Arc::clone(&backend);
                spawn_query(id, &events, &stop, async move { backend.query(&sql).await });
            }
            Request::Structure { id, schema, table } => {
                let backend = Arc::clone(&backend);
                spawn_query(id, &events, &stop, async move {
                    backend.structure(&schema, &table).await
                });
            }
            Request::Tables => spawn_tables(&backend, &events, &stop),
            Request::Cancel => {
                tokio::spawn(backend.canceller().cancel());
            }
        }
    }
    // Сначала соединение, потом туннель под ним.
    drop(backend);
    drop(tunnel);
}

fn spawn_query(
    id: u64,
    events: &Sender<Event>,
    stop: &CancellationToken,
    job: impl Future<Output = Result<QueryOutput, DbError>> + Send + 'static,
) {
    let events = events.clone();
    let stop = stop.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let result = tokio::select! {
            _ = stop.cancelled() => return,
            result = job => result,
        };
        let _ = events
            .send(Event::QueryDone {
                id,
                result,
                elapsed: started.elapsed(),
            })
            .await;
    });
}

fn spawn_databases(backend: &Arc<Backend>, events: &Sender<Event>, stop: &CancellationToken) {
    let backend = Arc::clone(backend);
    let events = events.clone();
    let stop = stop.clone();
    tokio::spawn(async move {
        let result = tokio::select! {
            _ = stop.cancelled() => return,
            result = backend.databases() => result,
        };
        // Без списка баз вкладка обходится: переключатель просто не появится.
        if let Ok((names, current)) = result
            && !names.is_empty()
        {
            let _ = events.send(Event::Databases { names, current }).await;
        }
    });
}

fn spawn_tables(backend: &Arc<Backend>, events: &Sender<Event>, stop: &CancellationToken) {
    let backend = Arc::clone(backend);
    let events = events.clone();
    let stop = stop.clone();
    tokio::spawn(async move {
        let result = tokio::select! {
            _ = stop.cancelled() => return,
            result = backend.tables() => result,
        };
        let _ = events.send(Event::Tables(result)).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_errors() {
        assert!(is_auth_error(
            "FATAL: password authentication failed for user \"app\""
        ));
        assert!(is_auth_error(
            "Access denied for user 'root'@'1.2.3.4' (using password: YES) (код 1045, SQLSTATE 28000)"
        ));
        assert!(is_auth_error("Сервер требует пароль, а он не указан"));
        assert!(!is_auth_error("Сервер не ответил за 20 с"));
    }

    #[test]
    fn socket_names() {
        let mut profile = DbProfile::template(DbKind::Postgres);
        profile.port = 6432;
        assert_eq!(socket_name(&profile), ".s.PGSQL.6432");
        assert_eq!(socket_name(&DbProfile::template(DbKind::MySql)), "db.sock");
    }

    /// Полный путь через воркер на настоящей (файловой) базе SQLite.
    #[test]
    fn sqlite_worker_end_to_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        {
            let conn = rusqlite::Connection::open(&path).expect("create");
            conn.execute_batch("CREATE TABLE t (a INTEGER); CREATE VIEW v AS SELECT a FROM t;")
                .expect("schema");
        }
        let profile = DbProfile {
            path: path.to_string_lossy().into_owned(),
            ..DbProfile::template(DbKind::Sqlite)
        }
        .normalized();
        let (tx, rx) = async_channel::unbounded();
        let worker = DbWorker::start(
            ConnectRequest {
                profile,
                password: String::new(),
                control: None,
            },
            tx,
        )
        .expect("start");

        let recv = || rx.recv_blocking().expect("event");
        assert!(matches!(recv(), Event::Connected { .. }));
        match recv() {
            Event::Tables(Ok(tables)) => {
                let names: Vec<_> = tables.iter().map(|t| (t.name.as_str(), t.view)).collect();
                assert_eq!(names, [("t", false), ("v", true)]);
            }
            other => panic!("ожидался список таблиц: {other:?}"),
        }
        worker.send(Request::Query {
            id: 7,
            sql: "INSERT INTO t VALUES (1), (2); SELECT sum(a) AS s FROM t".to_string(),
        });
        match recv() {
            Event::QueryDone {
                id: 7,
                result: Ok(output),
                ..
            } => {
                assert_eq!(output.columns, ["s"]);
                assert_eq!(output.rows, [vec![Some("3".to_string())]]);
            }
            other => panic!("ожидался результат: {other:?}"),
        }
        worker.send(Request::Structure {
            id: 8,
            schema: "main".to_string(),
            table: "t".to_string(),
        });
        match recv() {
            Event::QueryDone {
                id: 8,
                result: Ok(output),
                ..
            } => {
                assert_eq!(output.rows[0][0].as_deref(), Some("a"));
                assert_eq!(output.rows[0][1].as_deref(), Some("INTEGER"));
            }
            other => panic!("ожидалась структура: {other:?}"),
        }
    }

    #[test]
    fn missing_sqlite_file_is_reported() {
        let profile = DbProfile {
            path: "/nonexistent/dir/x.db".to_string(),
            ..DbProfile::template(DbKind::Sqlite)
        };
        let (tx, rx) = async_channel::unbounded();
        let _worker = DbWorker::start(
            ConnectRequest {
                profile,
                password: String::new(),
                control: None,
            },
            tx,
        )
        .expect("start");
        match rx.recv_blocking().expect("event") {
            Event::ConnectFailed(message) => assert!(message.contains("x.db"), "{message}"),
            other => panic!("ожидалась ошибка: {other:?}"),
        }
    }
}
