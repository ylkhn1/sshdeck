//! Клиент баз данных: профили подключений, SSH-туннели и драйверы.
//!
//! Как и SFTP, работает в своём tokio-рантайме и ничего не знает о GTK:
//! интерфейс отправляет [`Request`] через [`DbWorker::send`] и получает
//! [`Event`] из канала.

mod driver;
pub mod extension;
pub mod profile;
pub mod secrets;
pub mod sql;
pub mod transfer;
mod tunnel;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_channel::Sender;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use driver::{
    Catalog, ColumnInfo, DatabaseInfo, DbError, MAX_ROWS, QueryOutput, TableInfo,
    columns_from_structure,
};
pub use profile::{DbKind, DbProfile, Profiles};
pub use transfer::{ExportFormat, ImportOptions, TransferReport};

use driver::{Backend, ImportTarget};

/// Сколько ждать отмены запроса силами СУБД, прежде чем бросить его.
const CANCEL_GRACE: Duration = Duration::from_secs(3);

/// Что нужно для подключения.
pub struct ConnectRequest {
    pub profile: DbProfile,
    pub password: String,
    /// Сокет мастер-соединения открытой вкладки к SSH-серверу профиля.
    pub control: Option<PathBuf>,
}

/// `database` во всех запросах — база на сервере (PostgreSQL); `None` —
/// та, к которой подключились.
#[derive(Debug)]
pub enum Request {
    /// Выполнить SQL; `id` возвращается в [`Event::QueryDone`].
    Query {
        id: u64,
        database: Option<String>,
        sql: String,
    },
    /// Структура таблицы (колонки, типы); ответ — тоже `QueryDone`.
    Structure {
        id: u64,
        database: Option<String>,
        schema: String,
        table: String,
    },
    /// Схемы и таблицы базы; ответ — [`Event::Catalog`].
    Catalog { database: Option<String> },
    /// Колонки таблицы для дерева; ответ — [`Event::Columns`].
    Columns {
        database: Option<String>,
        schema: String,
        table: String,
    },
    /// Перечитать список баз.
    Databases,
    /// Выгрузить все строки первого результата `sql` в файл. `table` —
    /// имя для `INSERT INTO` (уже в кавычках).
    Export {
        id: u64,
        database: Option<String>,
        sql: String,
        path: PathBuf,
        format: ExportFormat,
        table: String,
    },
    /// Загрузить файл CSV или JSON в таблицу.
    Import {
        id: u64,
        database: Option<String>,
        schema: String,
        table: String,
        path: PathBuf,
        options: ImportOptions,
    },
    /// Прервать запрос, экспорт или импорт.
    Cancel { id: u64 },
}

#[derive(Debug)]
pub enum Event {
    Connected {
        /// Версия сервера, например «PostgreSQL 16.4».
        version: String,
        /// Как идёт соединение, для строки состояния.
        route: String,
        /// База, к которой подключились (PostgreSQL), иначе пусто.
        database: String,
    },
    ConnectFailed(String),
    Catalog {
        database: String,
        result: Result<Catalog, DbError>,
    },
    /// Базы на сервере (только PostgreSQL).
    Databases(Vec<DatabaseInfo>),
    Columns {
        database: String,
        schema: String,
        table: String,
        result: Result<Vec<ColumnInfo>, DbError>,
    },
    QueryDone {
        id: u64,
        result: Result<QueryOutput, DbError>,
        elapsed: Duration,
    },
    /// Импорт `id` вставил столько строк.
    Progress {
        id: u64,
        rows: u64,
    },
    TransferDone {
        id: u64,
        result: Result<TransferReport, DbError>,
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
        .map_err(|err| tr!("Could not start background threads: {}", err))?;
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

/// Ищет расширения в фоне; результат — список установленных.
pub fn scan_extensions() -> async_channel::Receiver<Vec<extension::ExtensionInfo>> {
    spawn(async {
        tokio::task::spawn_blocking(extension::scan)
            .await
            .unwrap_or_default()
    })
}

/// Похоже ли сообщение об ошибке подключения на неверный или пустой пароль.
pub fn is_auth_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    // Наши сообщения переведены на язык интерфейса; он не меняется до
    // перезапуска, поэтому сообщение и образец здесь на одном языке.
    // MySQL узнаётся по SQLSTATE 28000 (1045 и др.): это слово не
    // переводится.
    let no_password = tr!("The server requires a password, but none was given").to_lowercase();
    [
        "password authentication failed",
        "access denied",
        "sqlstate 28000",
        "authentication failed",
        no_password.as_str(),
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
    let tunnel = if profile.is_networked() && !profile.ssh.is_empty() {
        let target = crate::session::validate_target(&profile.ssh)?;
        let name = socket_name(profile);
        // Расширению нужен адрес и порт, а не Unix-сокет.
        let local = if profile.kind == DbKind::Extension {
            tunnel::Local::Tcp
        } else {
            tunnel::Local::Socket(&name)
        };
        Some(
            tunnel::open(tunnel::Spec {
                target: &target,
                control: request.control.as_deref(),
                host: &profile.host,
                port: profile.port,
                local,
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
        Some(tunnel) if tunnel.via_tab() => tr!("via the {} tab", profile.ssh),
        Some(_) => tr!("via SSH {}", profile.ssh),
        None if profile.is_networked() => {
            if profile.tls {
                tr!("direct, TLS").to_string()
            } else {
                tr!("direct").to_string()
            }
        }
        None => profile.summary(),
    };
    Ok((tunnel, backend, version, route))
}

/// Соединения вкладки: основное (редактор) и дополнительные — к другим
/// базам сервера, для дерева, экспорта и импорта.
struct Pool {
    main: Arc<Backend>,
    main_db: String,
    /// Соединения редактора к другим базам (PostgreSQL).
    work: tokio::sync::Mutex<HashMap<String, Arc<Backend>>>,
    /// Соединения для дерева: долгий запрос в редакторе его не держит.
    meta: tokio::sync::Mutex<HashMap<String, Arc<Backend>>>,
}

impl Pool {
    fn key(&self, database: Option<&str>) -> String {
        match database {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => self.main_db.clone(),
        }
    }

    /// Соединение для запросов редактора к базе.
    async fn work(&self, database: Option<&str>) -> Result<Arc<Backend>, DbError> {
        let key = self.key(database);
        if key == self.main_db {
            return Ok(Arc::clone(&self.main));
        }
        let mut work = self.work.lock().await;
        if let Some(backend) = work.get(&key) {
            return Ok(Arc::clone(backend));
        }
        let backend = match self.main.reopen(Some(&key)).await? {
            Some(backend) => Arc::new(backend),
            None => Arc::clone(&self.main),
        };
        work.insert(key, Arc::clone(&backend));
        Ok(backend)
    }

    /// Соединение для чтения каталога.
    async fn meta(&self, database: Option<&str>) -> Result<Arc<Backend>, DbError> {
        let key = self.key(database);
        let mut meta = self.meta.lock().await;
        if let Some(backend) = meta.get(&key) {
            return Ok(Arc::clone(backend));
        }
        let backend = match self.main.reopen(Some(&key)).await {
            Ok(Some(backend)) => {
                if backend.kind() == DbKind::Postgres {
                    // Таблица под блокировкой (миграция) не должна вешать
                    // дерево: размер ждёт блокировку не дольше 5 с.
                    let _ = backend.query("SET lock_timeout = '5s'").await;
                }
                Arc::new(backend)
            }
            // Второе соединение открыть нельзя или не удалось (лимит
            // соединений и т. п.) — дерево читается через основное.
            Ok(None) | Err(_) => self.work(database).await?,
        };
        meta.insert(key, Arc::clone(&backend));
        Ok(backend)
    }

    /// Отдельное соединение для экспорта или импорта: транзакция импорта не
    /// должна захватить запросы редактора.
    async fn fresh(&self, database: Option<&str>) -> Result<Arc<Backend>, DbError> {
        let key = self.key(database);
        match self.main.reopen(Some(&key)).await? {
            Some(backend) => Ok(Arc::new(backend)),
            None => self.work(database).await,
        }
    }
}

type RunningOps = HashMap<u64, (Arc<Backend>, CancellationToken)>;

/// Выполняющиеся операции: их можно прервать.
#[derive(Default, Clone)]
struct Running {
    ops: Arc<Mutex<RunningOps>>,
}

impl Running {
    fn add(&self, id: u64, backend: &Arc<Backend>, token: &CancellationToken) {
        if let Ok(mut ops) = self.ops.lock() {
            ops.insert(id, (Arc::clone(backend), token.clone()));
        }
    }

    fn remove(&self, id: u64) {
        if let Ok(mut ops) = self.ops.lock() {
            ops.remove(&id);
        }
    }

    /// Просит СУБД прервать операцию; если та не прервалась за
    /// [`CANCEL_GRACE`] (или `at_once` — импорт и экспорт), бросает её.
    fn cancel(&self, id: u64, at_once: bool) {
        let Some((backend, token)) = self.ops.lock().ok().and_then(|ops| ops.get(&id).cloned())
        else {
            return;
        };
        tokio::spawn(backend.canceller().cancel());
        if at_once {
            token.cancel();
        } else {
            tokio::spawn(async move {
                tokio::time::sleep(CANCEL_GRACE).await;
                token.cancel();
            });
        }
    }
}

struct Worker {
    pool: Arc<Pool>,
    events: Sender<Event>,
    stop: CancellationToken,
    running: Running,
    /// Номера операций экспорта и импорта (их прерывают сразу).
    transfers: Arc<Mutex<HashSet<u64>>>,
}

fn cancelled() -> DbError {
    DbError::new(tr!("Cancelled"))
}

/// Что выполнить в соединении редактора.
enum QueryJob {
    Sql(String),
    Structure { schema: String, table: String },
}

impl QueryJob {
    async fn run(self, backend: &Backend) -> Result<QueryOutput, DbError> {
        match self {
            QueryJob::Sql(sql) => backend.query(&sql).await,
            QueryJob::Structure { schema, table } => backend.structure(&schema, &table).await,
        }
    }
}

struct Export {
    sql: String,
    path: PathBuf,
    format: ExportFormat,
    table: String,
}

struct Import {
    schema: String,
    table: String,
    path: PathBuf,
    options: ImportOptions,
}

impl Worker {
    fn spawn(&self, job: impl Future<Output = ()> + Send + 'static) {
        let stop = self.stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = stop.cancelled() => {}
                () = job => {}
            }
        });
    }

    fn query(&self, id: u64, database: Option<String>, job: QueryJob) {
        let pool = Arc::clone(&self.pool);
        let events = self.events.clone();
        let running = self.running.clone();
        self.spawn(async move {
            let started = Instant::now();
            let result = async {
                let backend = pool.work(database.as_deref()).await?;
                let token = CancellationToken::new();
                running.add(id, &backend, &token);
                let result = tokio::select! {
                    _ = token.cancelled() => Err(cancelled()),
                    result = job.run(&backend) => result,
                };
                running.remove(id);
                result
            }
            .await;
            let _ = events
                .send(Event::QueryDone {
                    id,
                    result,
                    elapsed: started.elapsed(),
                })
                .await;
        });
    }

    fn catalog(&self, database: Option<String>) {
        let pool = Arc::clone(&self.pool);
        let events = self.events.clone();
        self.spawn(async move {
            let key = pool.key(database.as_deref());
            let result = match pool.meta(database.as_deref()).await {
                Ok(backend) => backend.catalog().await,
                Err(err) => Err(err),
            };
            let _ = events
                .send(Event::Catalog {
                    database: key,
                    result,
                })
                .await;
        });
    }

    fn databases(&self) {
        let pool = Arc::clone(&self.pool);
        let events = self.events.clone();
        self.spawn(async move {
            let result = match pool.meta(None).await {
                Ok(backend) => backend.databases().await,
                Err(err) => Err(err),
            };
            // Без списка баз вкладка обходится: в дереве будет одна база.
            if let Ok(list) = result
                && !list.is_empty()
            {
                let _ = events.send(Event::Databases(list)).await;
            }
        });
    }

    fn columns(&self, database: Option<String>, schema: String, table: String) {
        let pool = Arc::clone(&self.pool);
        let events = self.events.clone();
        self.spawn(async move {
            let key = pool.key(database.as_deref());
            let result = match pool.meta(database.as_deref()).await {
                Ok(backend) => backend
                    .structure(&schema, &table)
                    .await
                    .map(|output| columns_from_structure(&output)),
                Err(err) => Err(err),
            };
            let _ = events
                .send(Event::Columns {
                    database: key,
                    schema,
                    table,
                    result,
                })
                .await;
        });
    }

    /// Общая часть экспорта и импорта: отдельное соединение, отмена, итог.
    fn transfer<F, Fut>(&self, id: u64, database: Option<String>, job: F)
    where
        F: FnOnce(Arc<Backend>, Arc<Pool>, Sender<Event>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<TransferReport, DbError>> + Send,
    {
        let pool = Arc::clone(&self.pool);
        let events = self.events.clone();
        let running = self.running.clone();
        if let Ok(mut transfers) = self.transfers.lock() {
            transfers.insert(id);
        }
        let transfers = Arc::clone(&self.transfers);
        self.spawn(async move {
            let started = Instant::now();
            let result = async {
                let backend = pool.fresh(database.as_deref()).await?;
                let token = CancellationToken::new();
                running.add(id, &backend, &token);
                let result = tokio::select! {
                    _ = token.cancelled() => Err(cancelled()),
                    result = job(backend, Arc::clone(&pool), events.clone()) => result,
                };
                running.remove(id);
                result
            }
            .await;
            if let Ok(mut transfers) = transfers.lock() {
                transfers.remove(&id);
            }
            let _ = events
                .send(Event::TransferDone {
                    id,
                    result,
                    elapsed: started.elapsed(),
                })
                .await;
        });
    }

    fn export(&self, id: u64, database: Option<String>, export: Export) {
        self.transfer(id, database, move |backend, _, _| async move {
            let Export {
                sql,
                path,
                format,
                table,
            } = export;
            let writer = transfer::create_writer(&path, format, &table, backend.kind())
                .map_err(|err| DbError::new(format!("{}: {err}", path.display())))?;
            match backend.export(&sql, writer).await {
                Ok(rows) => Ok(TransferReport {
                    rows,
                    path,
                    skipped: Vec::new(),
                    note: None,
                }),
                Err(err) => {
                    // Недописанный файл не нужен.
                    let _ = std::fs::remove_file(&path);
                    Err(err)
                }
            }
        });
    }

    fn import(&self, id: u64, database: Option<String>, import: Import) {
        let meta_db = database.clone();
        self.transfer(id, database, move |backend, pool, events| async move {
            let Import {
                schema,
                table,
                path,
                options,
            } = import;
            // Колонки таблицы — для сопоставления с колонками файла.
            let table_columns: Vec<String> = pool
                .meta(meta_db.as_deref())
                .await?
                .structure(&schema, &table)
                .await
                .map(|output| columns_from_structure(&output))?
                .into_iter()
                .map(|column| column.name)
                .collect();
            if table_columns.is_empty() {
                return Err(DbError::new(tr!(
                    "Could not read the columns of table {}",
                    table
                )));
            }
            let bool_text = match backend.kind() {
                DbKind::MySql | DbKind::Sqlite => ("1", "0"),
                _ => ("true", "false"),
            };
            let file_path = path.clone();
            let (mut file, first) = tokio::task::spawn_blocking(move || {
                let mut file = transfer::ImportFile::open(&file_path, options)?;
                let first = file.next_row(bool_text)?;
                Ok::<_, String>((file, first))
            })
            .await
            .map_err(|_| DbError::new(tr!("Reading the file was interrupted")))?
            .map_err(DbError::new)?;
            let width = first.as_ref().map_or(0, Vec::len);
            let map = transfer::ColumnMap::build(file.columns.as_deref(), &table_columns, width)
                .map_err(DbError::new)?;

            // Файл читается в отдельном потоке и отдаётся порциями.
            let (tx, rx) = mpsc::channel::<driver::Chunk>(4);
            let reader_map = map.clone();
            tokio::task::spawn_blocking(move || {
                let mut chunk = Vec::with_capacity(transfer::CHUNK_ROWS);
                let mut next = first;
                while let Some(row) = next {
                    chunk.push(reader_map.apply(&row));
                    if chunk.len() >= transfer::CHUNK_ROWS {
                        let full =
                            std::mem::replace(&mut chunk, Vec::with_capacity(transfer::CHUNK_ROWS));
                        if tx.blocking_send(Ok(full)).is_err() {
                            return;
                        }
                    }
                    next = match file.next_row(bool_text) {
                        Ok(row) => row,
                        Err(message) => {
                            let _ = tx.blocking_send(Err(message));
                            return;
                        }
                    };
                }
                if !chunk.is_empty() {
                    let _ = tx.blocking_send(Ok(chunk));
                }
            });

            let progress: driver::Progress = Arc::new(move |rows| {
                let _ = events.try_send(Event::Progress { id, rows });
            });
            let rows = backend
                .import(
                    ImportTarget {
                        schema,
                        table,
                        columns: map.target.clone(),
                        truncate: options.truncate,
                    },
                    rx,
                    progress,
                )
                .await?;
            Ok(TransferReport {
                rows,
                path,
                skipped: map.skipped,
                note: None,
            })
        });
    }

    fn cancel(&self, id: u64) {
        let transfer = self
            .transfers
            .lock()
            .is_ok_and(|transfers| transfers.contains(&id));
        self.running.cancel(id, transfer);
    }
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
    let main_db = backend.database().to_string();
    let worker = Worker {
        pool: Arc::new(Pool {
            main: Arc::new(backend),
            main_db: main_db.clone(),
            work: tokio::sync::Mutex::new(HashMap::new()),
            meta: tokio::sync::Mutex::new(HashMap::new()),
        }),
        events: events.clone(),
        stop: stop.clone(),
        running: Running::default(),
        transfers: Arc::default(),
    };
    let _ = events
        .send(Event::Connected {
            version,
            route,
            database: main_db,
        })
        .await;
    worker.catalog(None);
    worker.databases();

    loop {
        let request = tokio::select! {
            _ = stop.cancelled() => break,
            request = rx.recv() => match request {
                Some(request) => request,
                None => break,
            },
        };
        match request {
            Request::Query { id, database, sql } => {
                worker.query(id, database, QueryJob::Sql(sql));
            }
            Request::Structure {
                id,
                database,
                schema,
                table,
            } => worker.query(id, database, QueryJob::Structure { schema, table }),
            Request::Catalog { database } => worker.catalog(database),
            Request::Columns {
                database,
                schema,
                table,
            } => worker.columns(database, schema, table),
            Request::Databases => worker.databases(),
            Request::Export {
                id,
                database,
                sql,
                path,
                format,
                table,
            } => worker.export(
                id,
                database,
                Export {
                    sql,
                    path,
                    format,
                    table,
                },
            ),
            Request::Import {
                id,
                database,
                schema,
                table,
                path,
                options,
            } => worker.import(
                id,
                database,
                Import {
                    schema,
                    table,
                    path,
                    options,
                },
            ),
            Request::Cancel { id } => worker.cancel(id),
        }
    }
    // Задачи с соединениями останавливаются по `stop`; туннель под ними
    // закрывается после короткой паузы.
    drop(worker);
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(tunnel);
}

// Объявлен здесь, а не наверху: всё, что выше первого `#[cfg(test)]`,
// проверяется тестами переводов (i18n.rs).
#[cfg(test)]
mod live_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_errors() {
        assert!(is_auth_error(
            "FATAL: password authentication failed for user \"app\""
        ));
        assert!(is_auth_error(
            "Access denied for user 'root'@'1.2.3.4' (using password: YES) (code 1045, SQLSTATE 28000)"
        ));
        assert!(is_auth_error("Accès refusé (code 1698, SQLSTATE 28000)"));
        assert!(is_auth_error("The server requires a password, but none was given"));
        assert!(!is_auth_error("The server did not respond within 20 s"));
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
            Event::Catalog {
                result: Ok(catalog),
                ..
            } => {
                let names: Vec<_> = catalog
                    .tables
                    .iter()
                    .map(|t| (t.name.as_str(), t.view))
                    .collect();
                assert_eq!(names, [("t", false), ("v", true)]);
                assert_eq!(catalog.schemas, ["main"]);
            }
            other => panic!("ожидался список таблиц: {other:?}"),
        }
        worker.send(Request::Query {
            id: 7,
            database: None,
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
            database: None,
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
        worker.send(Request::Columns {
            database: None,
            schema: "main".to_string(),
            table: "t".to_string(),
        });
        match recv() {
            Event::Columns {
                result: Ok(columns),
                table,
                ..
            } => {
                assert_eq!(table, "t");
                assert_eq!(columns[0].name, "a");
            }
            other => panic!("ожидались колонки: {other:?}"),
        }
    }

    /// Ждёт итога операции `id`, пропуская прогресс.
    fn transfer_result(
        rx: &async_channel::Receiver<Event>,
        id: u64,
    ) -> Result<TransferReport, DbError> {
        loop {
            match rx.recv_blocking().expect("event") {
                Event::TransferDone {
                    id: done, result, ..
                } if done == id => return result,
                Event::Progress { .. } | Event::Catalog { .. } => {}
                other => panic!("неожиданное событие: {other:?}"),
            }
        }
    }

    /// Экспорт таблицы в каждый формат и обратный импорт CSV и JSON.
    #[test]
    fn sqlite_export_and_import() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shop.db");
        {
            let conn = rusqlite::Connection::open(&path).expect("create");
            conn.execute_batch(
                "CREATE TABLE goods (id INTEGER PRIMARY KEY, name TEXT, price REAL, note TEXT);
                 INSERT INTO goods VALUES (1, 'Чай', 2.5, NULL), (2, 'Кофе; \"зерно\"', 10, '');
                 CREATE TABLE copy (id INTEGER PRIMARY KEY, name TEXT, price REAL, note TEXT);",
            )
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
        assert!(matches!(
            rx.recv_blocking().unwrap(),
            Event::Connected { .. }
        ));

        let mut id = 0;
        for format in ExportFormat::ALL {
            id += 1;
            let out = dir
                .path()
                .join(format!("goods-{id}.{}", format.extension()));
            worker.send(Request::Export {
                id,
                database: None,
                sql: "SELECT * FROM goods ORDER BY id".to_string(),
                path: out.clone(),
                format,
                table: "\"goods\"".to_string(),
            });
            let report = transfer_result(&rx, id).expect("export");
            assert_eq!(report.rows, 2);
            let text = std::fs::read_to_string(&out).expect("file");
            assert!(text.contains("Чай"), "{format:?}: {text}");
            if format == ExportFormat::Json {
                assert!(text.contains("\"price\":2.5"), "{text}");
            }
        }

        // CSV обратно в пустую таблицу: NULL и пустая строка различаются.
        id += 1;
        worker.send(Request::Import {
            id,
            database: None,
            schema: "main".to_string(),
            table: "copy".to_string(),
            path: dir.path().join("goods-1.csv"),
            options: ImportOptions::default(),
        });
        assert_eq!(transfer_result(&rx, id).expect("import csv").rows, 2);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let notes: Vec<Option<String>> = conn
                .prepare("SELECT note FROM copy ORDER BY id")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(notes, [None, Some(String::new())]);
        }

        // JSON с лишней колонкой — поверх, с очисткой таблицы.
        let json = dir.path().join("more.json");
        std::fs::write(
            &json,
            r#"[{"id": 5, "name": "Сок", "price": 3, "extra": 1}, {"id": 6, "name": null}]"#,
        )
        .unwrap();
        id += 1;
        worker.send(Request::Import {
            id,
            database: None,
            schema: "main".to_string(),
            table: "copy".to_string(),
            path: json.clone(),
            options: ImportOptions {
                truncate: true,
                ..ImportOptions::default()
            },
        });
        let report = transfer_result(&rx, id).expect("import json");
        assert_eq!(report.rows, 2);
        assert_eq!(report.skipped, ["extra"]);

        // Ошибка посреди файла откатывает всё: таблица не меняется.
        let broken = dir.path().join("broken.csv");
        std::fs::write(&broken, "id,name\n7,a\n5,дубль\n").unwrap();
        id += 1;
        worker.send(Request::Import {
            id,
            database: None,
            schema: "main".to_string(),
            table: "copy".to_string(),
            path: broken,
            options: ImportOptions::default(),
        });
        let error = transfer_result(&rx, id).expect_err("duplicate key");
        assert!(error.message.contains("UNIQUE"), "{}", error.message);

        id += 1;
        worker.send(Request::Query {
            id,
            database: None,
            sql: "SELECT id, name, price FROM copy ORDER BY id".to_string(),
        });
        loop {
            match rx.recv_blocking().unwrap() {
                Event::QueryDone { result, .. } => {
                    let output = result.expect("select");
                    assert_eq!(
                        output.rows,
                        [
                            vec![Some("5".into()), Some("Сок".into()), Some("3".into())],
                            vec![Some("6".into()), None, None],
                        ]
                    );
                    break;
                }
                Event::Catalog { .. } => {}
                other => panic!("неожиданное событие: {other:?}"),
            }
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
