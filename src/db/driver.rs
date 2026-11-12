//! Драйверы СУБД за общим интерфейсом: подключение, выполнение SQL,
//! список таблиц и структура таблицы.
//!
//! Значения всех типов приводятся к тексту: для просмотра этого достаточно,
//! и не нужно сопоставлять сотни типов каждой СУБД. PostgreSQL для этого
//! опрашивается по простому протоколу (сервер сам присылает текст), MySQL —
//! по текстовому, у SQLite типов всего пять.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use mysql_async::prelude::Queryable;
use rusqlite::fallible_iterator::FallibleIterator;
use tokio_postgres::SimpleQueryMessage;

use super::profile::{DbKind, DbProfile};
use super::tunnel::Tunnel;

/// Больше строк одного результата интерфейс не получает.
pub const MAX_ROWS: usize = 1000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Двоичные значения показываются шестнадцатерично, но не длиннее этого.
const BLOB_PREVIEW_BYTES: usize = 64;
const APP_NAME: &str = "sshdeck";

/// Результат запроса. Если запросов было несколько, это результат
/// последнего, вернувшего строки, а если таких нет — последнего запроса.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryOutput {
    /// Пусто — запрос не возвращает строк (`UPDATE`, `CREATE`…).
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    /// Строк было больше [`MAX_ROWS`].
    pub truncated: bool,
    /// Сколько строк изменил запрос без результата.
    pub affected: Option<u64>,
    /// Сколько запросов выполнено.
    pub statements: usize,
}

impl QueryOutput {
    fn table(columns: Vec<String>) -> Self {
        Self {
            columns,
            ..Self::default()
        }
    }

    /// Добавляет строку; `false` — лимит исчерпан и строка отброшена.
    fn push(&mut self, row: Vec<Option<String>>) -> bool {
        if self.rows.len() >= MAX_ROWS {
            self.truncated = true;
            return false;
        }
        self.rows.push(row);
        true
    }
}

/// Собирает итог из нескольких запросов.
#[derive(Default)]
struct Collector {
    last_table: Option<QueryOutput>,
    last_affected: Option<u64>,
    statements: usize,
}

impl Collector {
    fn table(&mut self, output: QueryOutput) {
        self.statements += 1;
        self.last_table = Some(output);
    }

    fn affected(&mut self, rows: u64) {
        self.statements += 1;
        self.last_affected = Some(rows);
    }

    fn finish(self) -> QueryOutput {
        let mut output = self.last_table.unwrap_or_else(|| QueryOutput {
            affected: self.last_affected,
            ..QueryOutput::default()
        });
        output.statements = self.statements;
        output
    }
}

/// Таблица или представление в навигаторе.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub view: bool,
}

/// Ошибка СУБД; `lost` — соединение потеряно и нужно переподключиться.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbError {
    pub message: String,
    pub lost: bool,
}

impl DbError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            lost: false,
        }
    }
}

type DbResult<T> = Result<T, DbError>;

/// Заголовки результата «структура таблицы».
fn structure_columns() -> Vec<String> {
    ["Колонка", "Тип", "NULL", "По умолчанию", "Ключ"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Байты как текст: UTF-8 как есть, остальное — `0x…`.
fn bytes_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => {
            let mut text = String::from("0x");
            for byte in bytes.iter().take(BLOB_PREVIEW_BYTES) {
                text.push_str(&format!("{byte:02x}"));
            }
            if bytes.len() > BLOB_PREVIEW_BYTES {
                text.push_str(&format!("… ({} Б)", bytes.len()));
            }
            text
        }
    }
}

pub enum Backend {
    Postgres(Postgres),
    MySql(MySql),
    Sqlite(Sqlite),
}

/// Отмена выполняющегося запроса; отдельное значение, чтобы не ждать
/// занятого соединения.
pub enum Canceller {
    Postgres(
        tokio_postgres::CancelToken,
        postgres_native_tls::MakeTlsConnector,
    ),
    MySql(mysql_async::Opts, u32),
    Sqlite(Arc<rusqlite::InterruptHandle>),
}

impl Canceller {
    pub async fn cancel(self) {
        match self {
            Canceller::Postgres(token, tls) => {
                let _ = token.cancel_query(tls).await;
            }
            Canceller::MySql(opts, id) => {
                if let Ok(mut conn) = mysql_async::Conn::new(opts).await {
                    let _ = conn.query_drop(format!("KILL QUERY {id}")).await;
                    let _ = conn.disconnect().await;
                }
            }
            Canceller::Sqlite(handle) => handle.interrupt(),
        }
    }
}

impl Backend {
    /// Подключается; второй элемент — версия сервера для строки состояния.
    pub async fn connect(
        profile: &DbProfile,
        password: &str,
        tunnel: Option<&Tunnel>,
    ) -> DbResult<(Self, String)> {
        let connect = async {
            match profile.kind {
                DbKind::Postgres => Postgres::connect(profile, password, tunnel).await,
                DbKind::MySql => MySql::connect(profile, password, tunnel).await,
                DbKind::Sqlite => Sqlite::open(profile).await,
            }
        };
        match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(result) => result,
            Err(_) => Err(DbError::new(format!(
                "Сервер не ответил за {} с",
                CONNECT_TIMEOUT.as_secs()
            ))),
        }
    }

    pub async fn query(&self, sql: &str) -> DbResult<QueryOutput> {
        match self {
            Backend::Postgres(pg) => pg.query(sql).await,
            Backend::MySql(my) => my.query(sql).await,
            Backend::Sqlite(lite) => lite.query(sql).await,
        }
    }

    pub async fn tables(&self) -> DbResult<Vec<TableInfo>> {
        match self {
            Backend::Postgres(pg) => pg.tables().await,
            Backend::MySql(my) => my.tables().await,
            Backend::Sqlite(lite) => lite.tables().await,
        }
    }

    /// Список баз на сервере и текущая база — для переключателя во вкладке.
    /// Нужен только PostgreSQL: в MySQL базы видны как схемы, а в SQLite
    /// база одна.
    pub async fn databases(&self) -> DbResult<(Vec<String>, String)> {
        match self {
            Backend::Postgres(pg) => pg.databases().await,
            Backend::MySql(_) | Backend::Sqlite(_) => Ok((Vec::new(), String::new())),
        }
    }

    pub async fn structure(&self, schema: &str, table: &str) -> DbResult<QueryOutput> {
        match self {
            Backend::Postgres(pg) => pg.structure(schema, table).await,
            Backend::MySql(my) => my.structure(schema, table).await,
            Backend::Sqlite(lite) => lite.structure(table).await,
        }
    }

    pub fn canceller(&self) -> Canceller {
        match self {
            Backend::Postgres(pg) => Canceller::Postgres(pg.client.cancel_token(), pg.tls.clone()),
            Backend::MySql(my) => Canceller::MySql(my.opts.clone(), my.id),
            Backend::Sqlite(lite) => Canceller::Sqlite(Arc::clone(&lite.interrupt)),
        }
    }
}

// ---------------------------------------------------------------- PostgreSQL

pub struct Postgres {
    client: tokio_postgres::Client,
    tls: postgres_native_tls::MakeTlsConnector,
}

fn pg_error(err: tokio_postgres::Error) -> DbError {
    let lost = err.is_closed();
    let Some(db) = err.as_db_error() else {
        let mut message = err.to_string();
        if message.contains("password missing") {
            return DbError::new("Сервер требует пароль, а он не указан");
        }
        if let Some(source) = std::error::Error::source(&err) {
            let source = source.to_string();
            if !message.contains(&source) {
                message = format!("{message}: {source}");
            }
        }
        return DbError { message, lost };
    };
    let mut message = format!("{}: {}", db.severity(), db.message());
    if let Some(detail) = db.detail() {
        message.push_str(&format!("\nПодробности: {detail}"));
    }
    if let Some(hint) = db.hint() {
        message.push_str(&format!("\nПодсказка: {hint}"));
    }
    if let Some(tokio_postgres::error::ErrorPosition::Original(position)) = db.position() {
        message.push_str(&format!("\nПозиция: {position}"));
    }
    DbError { message, lost }
}

impl Postgres {
    async fn connect(
        profile: &DbProfile,
        password: &str,
        tunnel: Option<&Tunnel>,
    ) -> DbResult<(Backend, String)> {
        let mut config = tokio_postgres::Config::new();
        let user = if profile.user.is_empty() {
            DbKind::Postgres.default_user()
        } else {
            &profile.user
        };
        config
            .user(user)
            .application_name(APP_NAME)
            .connect_timeout(CONNECT_TIMEOUT)
            .port(profile.port);
        if !profile.database.is_empty() {
            config.dbname(&profile.database);
        }
        if !password.is_empty() {
            config.password(password);
        }
        match tunnel {
            Some(tunnel) => {
                config.host_path(tunnel.dir());
            }
            None => {
                config.host(&profile.host);
            }
        }
        // Как `sslmode=prefer` и `require` в libpq: шифрование без проверки
        // сертификата (самоподписанные сертификаты на своих серверах —
        // обычное дело).
        config.ssl_mode(if profile.tls {
            tokio_postgres::config::SslMode::Require
        } else {
            tokio_postgres::config::SslMode::Prefer
        });
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .map_err(|err| DbError::new(format!("TLS: {err}")))?;
        let tls = postgres_native_tls::MakeTlsConnector::new(connector);
        let (client, connection) = config.connect(tls.clone()).await.map_err(pg_error)?;
        // Соединение обслуживается отдельной задачей и завершается вместе
        // с клиентом.
        tokio::spawn(connection);

        let version = client
            .simple_query("SHOW server_version")
            .await
            .ok()
            .and_then(|messages| {
                messages.into_iter().find_map(|message| match message {
                    SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
                    _ => None,
                })
            })
            .map_or_else(
                || "PostgreSQL".to_string(),
                |version| format!("PostgreSQL {version}"),
            );
        Ok((Backend::Postgres(Postgres { client, tls }), version))
    }

    async fn query(&self, sql: &str) -> DbResult<QueryOutput> {
        let stream = self.client.simple_query_raw(sql).await.map_err(pg_error)?;
        let mut stream = std::pin::pin!(stream);
        let mut collector = Collector::default();
        let mut current: Option<QueryOutput> = None;
        while let Some(message) = stream.next().await {
            match message.map_err(pg_error)? {
                SimpleQueryMessage::RowDescription(columns) => {
                    current = Some(QueryOutput::table(
                        columns.iter().map(|c| c.name().to_string()).collect(),
                    ));
                }
                SimpleQueryMessage::Row(row) => {
                    if let Some(output) = current.as_mut() {
                        let values = (0..row.len()).map(|i| row.get(i).map(str::to_string));
                        if !output.push(values.collect()) {
                            // Лишние строки не нужны: остаток ответа
                            // соединение дочитает и отбросит само.
                            collector.table(current.take().unwrap_or_default());
                            return Ok(collector.finish());
                        }
                    }
                }
                SimpleQueryMessage::CommandComplete(rows) => match current.take() {
                    Some(output) => collector.table(output),
                    None => collector.affected(rows),
                },
                _ => {}
            }
        }
        Ok(collector.finish())
    }

    /// Таблицы из системного каталога, а не из `information_schema`: та
    /// показывает только таблицы, на которые у пользователя есть права, и
    /// не знает о материализованных представлениях. Секции партиционированных
    /// таблиц не показываются — только сама таблица.
    async fn tables(&self) -> DbResult<Vec<TableInfo>> {
        let rows = self
            .client
            .query(
                "SELECT n.nspname::text, c.relname::text, c.relkind::text \
                 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') \
                   AND NOT c.relispartition \
                   AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
                   AND n.nspname NOT LIKE 'pg\\_toast%' \
                   AND n.nspname NOT LIKE 'pg\\_temp%' \
                 ORDER BY 1, 2",
                &[],
            )
            .await
            .map_err(pg_error)?;
        Ok(rows
            .iter()
            .map(|row| TableInfo {
                schema: row.get(0),
                name: row.get(1),
                view: matches!(row.get::<_, String>(2).as_str(), "v" | "m"),
            })
            .collect())
    }

    /// Базы на сервере, к которым можно подключиться, и текущая база.
    async fn databases(&self) -> DbResult<(Vec<String>, String)> {
        let rows = self
            .client
            .query(
                "SELECT datname::text, datname = current_database() \
                 FROM pg_catalog.pg_database \
                 WHERE datallowconn AND NOT datistemplate ORDER BY 1",
                &[],
            )
            .await
            .map_err(pg_error)?;
        let names: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
        let current = rows
            .iter()
            .find(|row| row.get::<_, bool>(1))
            .map(|row| row.get(0))
            .unwrap_or_default();
        Ok((names, current))
    }

    async fn structure(&self, schema: &str, table: &str) -> DbResult<QueryOutput> {
        let rows = self
            .client
            .query(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), \
                        CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END, \
                        pg_get_expr(d.adbin, d.adrelid), \
                        CASE WHEN EXISTS (SELECT 1 FROM pg_index i \
                                          WHERE i.indrelid = c.oid AND i.indisprimary \
                                            AND a.attnum = ANY(i.indkey)) \
                             THEN 'PK' ELSE '' END \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 \
                   AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&schema, &table],
            )
            .await
            .map_err(pg_error)?;
        let mut output = QueryOutput::table(structure_columns());
        for row in &rows {
            output.push((0..5).map(|i| row.get::<_, Option<String>>(i)).collect());
        }
        output.statements = 1;
        Ok(output)
    }
}

// --------------------------------------------------------------------- MySQL

pub struct MySql {
    conn: tokio::sync::Mutex<mysql_async::Conn>,
    opts: mysql_async::Opts,
    /// Номер соединения на сервере (для `KILL QUERY`).
    id: u32,
}

fn my_error(err: mysql_async::Error) -> DbError {
    match err {
        mysql_async::Error::Server(server) => DbError::new(format!(
            "{} (код {}, SQLSTATE {})",
            server.message, server.code, server.state
        )),
        mysql_async::Error::Io(io) => DbError {
            message: format!("Ошибка соединения: {io}"),
            lost: true,
        },
        other => DbError::new(other.to_string()),
    }
}

fn my_value(value: mysql_async::Value) -> Option<String> {
    use mysql_async::Value;
    Some(match value {
        Value::NULL => return None,
        Value::Bytes(bytes) => bytes_text(&bytes),
        Value::Int(v) => v.to_string(),
        Value::UInt(v) => v.to_string(),
        Value::Float(v) => v.to_string(),
        Value::Double(v) => v.to_string(),
        Value::Date(y, mo, d, h, mi, s, us) => {
            let mut text = format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}");
            if us > 0 {
                text.push_str(&format!(".{us:06}"));
            }
            text
        }
        Value::Time(negative, days, h, mi, s, us) => {
            let hours = u64::from(days) * 24 + u64::from(h);
            let sign = if negative { "-" } else { "" };
            let mut text = format!("{sign}{hours:02}:{mi:02}:{s:02}");
            if us > 0 {
                text.push_str(&format!(".{us:06}"));
            }
            text
        }
    })
}

fn my_row(row: mysql_async::Row) -> Vec<Option<String>> {
    row.unwrap().into_iter().map(my_value).collect()
}

impl MySql {
    async fn connect(
        profile: &DbProfile,
        password: &str,
        tunnel: Option<&Tunnel>,
    ) -> DbResult<(Backend, String)> {
        let user = if profile.user.is_empty() {
            DbKind::MySql.default_user()
        } else {
            &profile.user
        };
        let mut builder = mysql_async::OptsBuilder::default()
            .user(Some(user))
            .pass((!password.is_empty()).then_some(password))
            .db_name((!profile.database.is_empty()).then_some(profile.database.as_str()))
            .prefer_socket(false);
        builder = match tunnel {
            Some(tunnel) => builder.socket(Some(tunnel.socket().to_string_lossy())),
            None => builder
                .ip_or_hostname(profile.host.as_str())
                .tcp_port(profile.port),
        };
        if profile.tls && tunnel.is_none() {
            builder = builder.ssl_opts(
                mysql_async::SslOpts::default()
                    .with_danger_accept_invalid_certs(true)
                    .with_danger_skip_domain_validation(true),
            );
        }
        let opts = mysql_async::Opts::from(builder);
        let mut conn = mysql_async::Conn::new(opts.clone())
            .await
            .map_err(my_error)?;
        let id = conn.id();
        let version: Option<String> = conn.query_first("SELECT VERSION()").await.ok().flatten();
        let version = match version {
            Some(version) if version.contains("MariaDB") => format!("MariaDB {version}"),
            Some(version) => format!("MySQL {version}"),
            None => "MySQL".to_string(),
        };
        Ok((
            Backend::MySql(MySql {
                conn: tokio::sync::Mutex::new(conn),
                opts,
                id,
            }),
            version,
        ))
    }

    async fn query(&self, sql: &str) -> DbResult<QueryOutput> {
        let mut conn = self.conn.lock().await;
        let mut result = conn.query_iter(sql).await.map_err(my_error)?;
        let mut collector = Collector::default();
        loop {
            let columns: Vec<String> = result
                .columns_ref()
                .iter()
                .map(|column| column.name_str().into_owned())
                .collect();
            let mut output = QueryOutput::table(columns);
            // Строки сверх лимита дочитываются и отбрасываются: иначе
            // соединение останется занятым остатком ответа.
            while let Some(row) = result.next().await.map_err(my_error)? {
                if output.rows.len() < MAX_ROWS {
                    output.rows.push(my_row(row));
                } else {
                    output.truncated = true;
                }
            }
            if output.columns.is_empty() {
                collector.affected(result.affected_rows());
            } else {
                collector.table(output);
            }
            if result.is_empty() {
                break;
            }
        }
        Ok(collector.finish())
    }

    async fn tables(&self) -> DbResult<Vec<TableInfo>> {
        let mut conn = self.conn.lock().await;
        let rows: Vec<mysql_async::Row> = conn
            .query(
                "SELECT table_schema, table_name, table_type FROM information_schema.tables \
                 WHERE table_schema NOT IN ('mysql', 'information_schema', \
                                            'performance_schema', 'sys') \
                 ORDER BY 1, 2",
            )
            .await
            .map_err(my_error)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let mut values = my_row(row).into_iter().map(Option::unwrap_or_default);
                let schema = values.next().unwrap_or_default();
                let name = values.next().unwrap_or_default();
                let kind = values.next().unwrap_or_default();
                TableInfo {
                    schema,
                    name,
                    view: kind.contains("VIEW"),
                }
            })
            .collect())
    }

    async fn structure(&self, schema: &str, table: &str) -> DbResult<QueryOutput> {
        let mut conn = self.conn.lock().await;
        let rows: Vec<mysql_async::Row> = conn
            .exec(
                "SELECT column_name, column_type, is_nullable, column_default, column_key \
                 FROM information_schema.columns \
                 WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position",
                (schema, table),
            )
            .await
            .map_err(my_error)?;
        let mut output = QueryOutput::table(structure_columns());
        for row in rows {
            output.push(my_row(row));
        }
        output.statements = 1;
        Ok(output)
    }
}

// -------------------------------------------------------------------- SQLite

pub struct Sqlite {
    conn: Arc<Mutex<rusqlite::Connection>>,
    interrupt: Arc<rusqlite::InterruptHandle>,
}

fn lite_error(err: rusqlite::Error) -> DbError {
    DbError::new(err.to_string())
}

fn lite_value(value: rusqlite::types::ValueRef<'_>) -> Option<String> {
    use rusqlite::types::ValueRef;
    Some(match value {
        ValueRef::Null => return None,
        ValueRef::Integer(v) => v.to_string(),
        ValueRef::Real(v) => v.to_string(),
        ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        // Текст в BLOB-колонке тоже бывает: он показывается как есть.
        ValueRef::Blob(bytes) => bytes_text(bytes),
    })
}

/// Строки запроса (до лимита).
fn lite_rows(statement: &mut rusqlite::Statement<'_>) -> rusqlite::Result<QueryOutput> {
    let count = statement.column_count();
    let columns = statement.column_names().map(str::to_string).collect();
    let mut output = QueryOutput::table(columns);
    let mut rows = statement.raw_query();
    while let Some(row) = rows.next()? {
        let values = (0..count).map(|i| row.get_ref(i).ok().and_then(lite_value));
        if !output.push(values.collect()) {
            break;
        }
    }
    Ok(output)
}

fn lite_query(conn: &rusqlite::Connection, sql: &str) -> rusqlite::Result<QueryOutput> {
    let mut collector = Collector::default();
    let mut batch = rusqlite::Batch::new(conn, sql);
    while let Some(mut statement) = batch.next()? {
        if statement.column_count() > 0 {
            collector.table(lite_rows(&mut statement)?);
        } else {
            statement.raw_execute()?;
            collector.affected(conn.changes());
        }
    }
    Ok(collector.finish())
}

impl Sqlite {
    async fn open(profile: &DbProfile) -> DbResult<(Backend, String)> {
        let path = std::path::PathBuf::from(&profile.path);
        let conn = tokio::task::spawn_blocking(move || {
            use rusqlite::OpenFlags;
            // Без SQLITE_OPEN_CREATE: опечатка в пути не должна молча
            // создавать пустую базу.
            let conn = rusqlite::Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|err| match err.sqlite_error_code() {
                Some(rusqlite::ErrorCode::CannotOpen) => DbError::new(format!(
                    "Не удалось открыть {}: файла нет или нет доступа",
                    path.display()
                )),
                _ => lite_error(err),
            })?;
            conn.busy_timeout(Duration::from_secs(5))
                .map_err(lite_error)?;
            // Проверка, что это действительно база SQLite.
            conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
                .map_err(lite_error)?;
            Ok::<_, DbError>(conn)
        })
        .await
        .map_err(|_| DbError::new("Открытие базы прервалось"))??;
        let interrupt = Arc::new(conn.get_interrupt_handle());
        let version = format!("SQLite {}", rusqlite::version());
        Ok((
            Backend::Sqlite(Sqlite {
                conn: Arc::new(Mutex::new(conn)),
                interrupt,
            }),
            version,
        ))
    }

    /// Выполняет `job` с соединением в потоке для блокирующих вызовов.
    async fn with_conn<T: Send + 'static>(
        &self,
        job: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> DbResult<T> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            job(&conn).map_err(lite_error)
        })
        .await
        .map_err(|_| DbError::new("Запрос прервался из-за внутренней ошибки"))?
    }

    async fn query(&self, sql: &str) -> DbResult<QueryOutput> {
        let sql = sql.to_string();
        self.with_conn(move |conn| lite_query(conn, &sql)).await
    }

    async fn tables(&self) -> DbResult<Vec<TableInfo>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT name, type FROM sqlite_master \
                 WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
                 ORDER BY name",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(TableInfo {
                    schema: "main".to_string(),
                    name: row.get(0)?,
                    view: row.get::<_, String>(1)? == "view",
                })
            })?;
            rows.collect()
        })
        .await
    }

    async fn structure(&self, table: &str) -> DbResult<QueryOutput> {
        let table = table.to_string();
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(
                "SELECT name, type, CASE \"notnull\" WHEN 0 THEN 'YES' ELSE 'NO' END, \
                        dflt_value, CASE pk WHEN 0 THEN '' ELSE 'PK' END \
                 FROM pragma_table_info(?1)",
            )?;
            let mut output = QueryOutput::table(structure_columns());
            let mut rows = statement.query([&table])?;
            while let Some(row) = rows.next()? {
                output.push(
                    (0..5)
                        .map(|i| row.get_ref(i).ok().and_then(lite_value))
                        .collect(),
                );
            }
            output.statements = 1;
            Ok(output)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, pic BLOB);
             INSERT INTO users (name, score, pic) VALUES ('Аня', 1.5, x'00ff'), ('Боб', NULL, NULL);",
        )
        .expect("schema");
        conn
    }

    #[test]
    fn sqlite_select_values_become_text() {
        let output = lite_query(&sqlite_conn(), "SELECT * FROM users ORDER BY id").expect("query");
        assert_eq!(output.columns, ["id", "name", "score", "pic"]);
        assert_eq!(
            output.rows,
            [
                vec![
                    Some("1".to_string()),
                    Some("Аня".to_string()),
                    Some("1.5".to_string()),
                    Some("0x00ff".to_string())
                ],
                vec![Some("2".to_string()), Some("Боб".to_string()), None, None],
            ]
        );
        assert!(!output.truncated);
        assert_eq!(output.statements, 1);
    }

    #[test]
    fn sqlite_several_statements_show_last_table() {
        let conn = sqlite_conn();
        let output = lite_query(
            &conn,
            "UPDATE users SET score = 2; SELECT name FROM users WHERE id = 2; DELETE FROM users WHERE id = 1",
        )
        .expect("query");
        assert_eq!(output.columns, ["name"]);
        assert_eq!(output.rows, [vec![Some("Боб".to_string())]]);
        assert_eq!(output.statements, 3);

        let output = lite_query(&conn, "UPDATE users SET score = 3").expect("update");
        assert!(output.columns.is_empty());
        assert_eq!(output.affected, Some(1));
    }

    #[test]
    fn sqlite_rows_are_limited() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        let output = lite_query(
            &conn,
            &format!(
                "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < {}) \
                 SELECT x FROM n",
                MAX_ROWS + 5
            ),
        )
        .expect("query");
        assert_eq!(output.rows.len(), MAX_ROWS);
        assert!(output.truncated);
    }

    #[test]
    fn sqlite_errors_are_reported() {
        assert!(lite_query(&sqlite_conn(), "SELECT * FROM nope").is_err());
    }

    #[test]
    fn mysql_values() {
        use mysql_async::Value;
        assert_eq!(my_value(Value::NULL), None);
        assert_eq!(
            my_value(Value::Bytes(b"abc".to_vec())),
            Some("abc".to_string())
        );
        assert_eq!(
            my_value(Value::Date(2026, 11, 12, 9, 5, 0, 0)),
            Some("2026-11-12 09:05:00".to_string())
        );
        assert_eq!(
            my_value(Value::Time(true, 1, 2, 3, 4, 5)),
            Some("-26:03:04.000005".to_string())
        );
    }

    #[test]
    fn binary_values_are_hex_and_capped() {
        assert_eq!(bytes_text(&[0xff, 0x00]), "0xff00");
        let long = vec![0xffu8; BLOB_PREVIEW_BYTES + 10];
        assert!(bytes_text(&long).ends_with(&format!("… ({} Б)", BLOB_PREVIEW_BYTES + 10)));
    }
}
