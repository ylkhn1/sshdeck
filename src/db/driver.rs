//! Драйверы СУБД за общим интерфейсом: подключение, выполнение SQL,
//! каталог (схемы, таблицы и их размеры), структура таблицы, экспорт и
//! импорт.
//!
//! Значения всех типов приводятся к тексту: для просмотра этого достаточно,
//! и не нужно сопоставлять сотни типов каждой СУБД. PostgreSQL для этого
//! опрашивается по простому протоколу (сервер сам присылает текст), MySQL —
//! по текстовому, у SQLite типов всего пять. Расширения присылают JSON.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mysql_async::prelude::Queryable;
use rusqlite::fallible_iterator::FallibleIterator;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_postgres::SimpleQueryMessage;

use super::extension::{self, ExtensionInfo, Plugin};
use super::profile::{DbKind, DbProfile};
use super::sql;
use super::transfer::{self, ExportColumn, RowWriter, Rows, ValueKind};
use super::tunnel::Tunnel;

/// Больше строк одного результата интерфейс не получает.
pub const MAX_ROWS: usize = 1000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Двоичные значения показываются шестнадцатерично, но не длиннее этого.
const BLOB_PREVIEW_BYTES: usize = 64;
const APP_NAME: &str = "sshdeck";
/// Больше параметров в одном `INSERT` MySQL не принимает (65535).
const MYSQL_MAX_PARAMS: usize = 60_000;

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
    /// Размер на диске вместе с индексами, байт.
    pub size: Option<u64>,
    /// Примерное число строк (по статистике СУБД).
    pub rows: Option<u64>,
}

/// Схемы и таблицы одной базы.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Все схемы, в том числе пустые.
    pub schemas: Vec<String>,
    pub tables: Vec<TableInfo>,
}

/// База на сервере (PostgreSQL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseInfo {
    pub name: String,
    pub size: Option<u64>,
    /// К ней подключена вкладка.
    pub current: bool,
}

/// Колонка таблицы для дерева.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub primary: bool,
}

/// Колонки из результата «структура таблицы».
pub fn columns_from_structure(output: &QueryOutput) -> Vec<ColumnInfo> {
    output
        .rows
        .iter()
        .map(|row| {
            let cell = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
            ColumnInfo {
                name: cell(0),
                type_name: cell(1),
                nullable: cell(2) != "NO",
                primary: cell(4).contains("PK") || cell(4) == "PRI",
            }
        })
        .collect()
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

fn write_error(err: std::io::Error) -> DbError {
    DbError::new(format!("Не удалось записать файл: {err}"))
}

/// Заголовки результата «структура таблицы».
fn structure_columns() -> Vec<String> {
    ["Колонка", "Тип", "NULL", "По умолчанию", "Ключ"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Байты как текст: UTF-8 как есть, остальное — `0x…` (для просмотра не
/// длиннее `limit` байт).
fn bytes_text(bytes: &[u8], limit: Option<usize>) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => {
            let shown = limit.unwrap_or(bytes.len()).min(bytes.len());
            let mut text = String::with_capacity(2 + shown * 2);
            text.push_str("0x");
            for byte in &bytes[..shown] {
                text.push_str(&format!("{byte:02x}"));
            }
            if shown < bytes.len() {
                text.push_str(&format!("… ({} Б)", bytes.len()));
            }
            text
        }
    }
}

/// Порция строк импорта или ошибка чтения файла.
pub type Chunk = Result<Rows, String>;
/// Сколько строк уже вставлено.
pub type Progress = Arc<dyn Fn(u64) + Send + Sync>;

/// Куда импортировать.
pub struct ImportTarget {
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub truncate: bool,
}

pub enum Backend {
    Postgres(Box<Postgres>),
    MySql(MySql),
    Sqlite(Sqlite),
    Extension(Extension),
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
    Extension(Arc<Plugin>),
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
            Canceller::Extension(plugin) => plugin.cancel().await,
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
                DbKind::Sqlite => Sqlite::open(&PathBuf::from(&profile.path)).await,
                DbKind::Extension => Extension::connect(profile, password, tunnel).await,
            }
        };
        with_timeout(connect).await
    }

    /// Ещё одно соединение с теми же параметрами (для дерева, экспорта и
    /// импорта, чтобы не занимать соединение редактора); для PostgreSQL —
    /// можно к другой базе. `None` — драйвер так не умеет (расширения:
    /// у них одно соединение).
    pub async fn reopen(&self, database: Option<&str>) -> DbResult<Option<Backend>> {
        let open = async {
            match self {
                Backend::Postgres(pg) => pg.reopen(database).await.map(Some),
                Backend::MySql(my) => my.reopen().await.map(Some),
                Backend::Sqlite(lite) => Sqlite::open(&lite.path)
                    .await
                    .map(|(backend, _)| Some(backend)),
                Backend::Extension(_) => Ok(None),
            }
        };
        with_timeout(open).await
    }

    /// База, к которой подключено соединение (PostgreSQL); иначе пусто.
    pub fn database(&self) -> &str {
        match self {
            Backend::Postgres(pg) => &pg.database,
            _ => "",
        }
    }

    pub fn kind(&self) -> DbKind {
        match self {
            Backend::Postgres(_) => DbKind::Postgres,
            Backend::MySql(_) => DbKind::MySql,
            Backend::Sqlite(_) => DbKind::Sqlite,
            Backend::Extension(_) => DbKind::Extension,
        }
    }

    pub async fn query(&self, sql: &str) -> DbResult<QueryOutput> {
        match self {
            Backend::Postgres(pg) => pg.query(sql).await,
            Backend::MySql(my) => my.query(sql).await,
            Backend::Sqlite(lite) => lite.query(sql).await,
            Backend::Extension(ext) => ext.query(sql).await,
        }
    }

    /// Схемы и таблицы с размерами. Если размеры прочитать не удалось
    /// (например, таблица заблокирована), список приходит без них.
    pub async fn catalog(&self) -> DbResult<Catalog> {
        match self {
            Backend::Postgres(pg) => match pg.catalog(true).await {
                Ok(catalog) => Ok(catalog),
                Err(err) if err.lost => Err(err),
                Err(_) => pg.catalog(false).await,
            },
            Backend::MySql(my) => my.catalog().await,
            Backend::Sqlite(lite) => lite.catalog().await,
            Backend::Extension(ext) => ext.catalog().await,
        }
    }

    /// Базы на сервере. Нужно только PostgreSQL: в MySQL базы видны как
    /// схемы, а в SQLite база одна.
    pub async fn databases(&self) -> DbResult<Vec<DatabaseInfo>> {
        match self {
            Backend::Postgres(pg) => pg.databases().await,
            _ => Ok(Vec::new()),
        }
    }

    pub async fn structure(&self, schema: &str, table: &str) -> DbResult<QueryOutput> {
        match self {
            Backend::Postgres(pg) => pg.structure(schema, table).await,
            Backend::MySql(my) => my.structure(schema, table).await,
            Backend::Sqlite(lite) => lite.structure(schema, table).await,
            Backend::Extension(ext) => ext.structure(schema, table).await,
        }
    }

    pub fn canceller(&self) -> Canceller {
        match self {
            Backend::Postgres(pg) => Canceller::Postgres(pg.client.cancel_token(), pg.tls.clone()),
            Backend::MySql(my) => Canceller::MySql(my.opts.clone(), my.id),
            Backend::Sqlite(lite) => Canceller::Sqlite(Arc::clone(&lite.interrupt)),
            Backend::Extension(ext) => Canceller::Extension(Arc::clone(&ext.plugin)),
        }
    }

    /// Выгружает все строки первого результата `sql` в `writer`; итог —
    /// число строк.
    pub async fn export(&self, sql: &str, writer: Box<dyn RowWriter>) -> DbResult<u64> {
        match self {
            Backend::Postgres(pg) => pg.export(sql, writer).await,
            Backend::MySql(my) => my.export(sql, writer).await,
            Backend::Sqlite(lite) => lite.export(sql, writer).await,
            Backend::Extension(ext) => ext.export(sql, writer).await,
        }
    }

    /// Вставляет строки из `chunks` одной транзакцией (у расширений — как
    /// решит расширение); итог — число строк.
    pub async fn import(
        &self,
        target: ImportTarget,
        chunks: mpsc::Receiver<Chunk>,
        progress: Progress,
    ) -> DbResult<u64> {
        match self {
            Backend::Postgres(pg) => pg.import(target, chunks, progress).await,
            Backend::MySql(my) => my.import(target, chunks, progress).await,
            Backend::Sqlite(lite) => lite.import(target, chunks, progress).await,
            Backend::Extension(ext) => ext.import(target, chunks, progress).await,
        }
    }
}

async fn with_timeout<T>(job: impl Future<Output = DbResult<T>>) -> DbResult<T> {
    match tokio::time::timeout(CONNECT_TIMEOUT, job).await {
        Ok(result) => result,
        Err(_) => Err(DbError::new(format!(
            "Сервер не ответил за {} с",
            CONNECT_TIMEOUT.as_secs()
        ))),
    }
}

fn target_name(target: &ImportTarget, kind: DbKind) -> (String, String) {
    let table = sql::qualified(&target.schema, &target.table, kind);
    let columns: Vec<String> = target
        .columns
        .iter()
        .map(|c| sql::quote_ident(c, kind))
        .collect();
    (table, columns.join(", "))
}

// ---------------------------------------------------------------- PostgreSQL

pub struct Postgres {
    client: tokio_postgres::Client,
    tls: postgres_native_tls::MakeTlsConnector,
    /// Параметры подключения: по ним открываются дополнительные соединения.
    config: tokio_postgres::Config,
    /// Текущая база.
    database: String,
    /// `server_version_num`, например 160004.
    version_num: i32,
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
    if let Some(where_) = db.where_() {
        message.push_str(&format!("\nГде: {where_}"));
    }
    if let Some(tokio_postgres::error::ErrorPosition::Original(position)) = db.position() {
        message.push_str(&format!("\nПозиция: {position}"));
    }
    DbError { message, lost }
}

fn pg_kind(ty: &tokio_postgres::types::Type) -> ValueKind {
    use tokio_postgres::types::Type;
    match *ty {
        Type::INT2
        | Type::INT4
        | Type::INT8
        | Type::OID
        | Type::FLOAT4
        | Type::FLOAT8
        | Type::NUMERIC => ValueKind::Number,
        Type::BOOL => ValueKind::Bool,
        _ => ValueKind::Text,
    }
}

/// Неотрицательное `bigint` как размер.
fn to_u64(value: Option<i64>) -> Option<u64> {
    value.and_then(|v| u64::try_from(v).ok())
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
        let pg = Self::open(config, tls).await?;
        let version = pg.version_label().await;
        Ok((Backend::Postgres(Box::new(pg)), version))
    }

    async fn open(
        config: tokio_postgres::Config,
        tls: postgres_native_tls::MakeTlsConnector,
    ) -> DbResult<Self> {
        let (client, connection) = config.connect(tls.clone()).await.map_err(pg_error)?;
        // Соединение обслуживается отдельной задачей и завершается вместе
        // с клиентом.
        tokio::spawn(connection);
        let (database, version_num) = client
            .query_one(
                "SELECT current_database()::text, current_setting('server_version_num')::int",
                &[],
            )
            .await
            .map(|row| (row.get(0), row.get(1)))
            .unwrap_or_default();
        Ok(Self {
            client,
            tls,
            config,
            database,
            version_num,
        })
    }

    async fn version_label(&self) -> String {
        self.client
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
            )
    }

    async fn reopen(&self, database: Option<&str>) -> DbResult<Backend> {
        let mut config = self.config.clone();
        if let Some(database) = database {
            config.dbname(database);
        }
        Ok(Backend::Postgres(Box::new(
            Self::open(config, self.tls.clone()).await?,
        )))
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

    /// Схемы и таблицы из системного каталога, а не из `information_schema`:
    /// та показывает только таблицы, на которые у пользователя есть права,
    /// и не знает о материализованных представлениях. Секции
    /// партиционированных таблиц не показываются — только сама таблица (её
    /// размер — сумма секций).
    async fn catalog(&self, sizes: bool) -> DbResult<Catalog> {
        let system = "n.nspname NOT IN ('pg_catalog', 'information_schema') \
                      AND n.nspname NOT LIKE 'pg\\_toast%' \
                      AND n.nspname NOT LIKE 'pg\\_temp%'";
        let schemas = self
            .client
            .query(
                &format!(
                    "SELECT n.nspname::text FROM pg_catalog.pg_namespace n \
                     WHERE {system} ORDER BY 1"
                ),
                &[],
            )
            .await
            .map_err(pg_error)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let partitions = self.version_num >= 120000;
        let (size, rows) = if sizes {
            let size_parts = if partitions {
                " WHEN c.relkind = 'p' THEN (SELECT sum(pg_catalog.pg_total_relation_size(t.relid))::bigint \
                   FROM pg_catalog.pg_partition_tree(c.oid) t)"
            } else {
                ""
            };
            let rows_parts = if partitions {
                " WHEN c.relkind = 'p' THEN (SELECT sum(greatest(p.reltuples, 0))::bigint \
                   FROM pg_catalog.pg_partition_tree(c.oid) t \
                   JOIN pg_catalog.pg_class p ON p.oid = t.relid WHERE t.isleaf)"
            } else {
                ""
            };
            (
                format!(
                    "CASE WHEN c.relkind IN ('r', 'm') \
                     THEN pg_catalog.pg_total_relation_size(c.oid){size_parts} END"
                ),
                format!(
                    "CASE WHEN c.relkind IN ('r', 'm') \
                     THEN CASE WHEN c.reltuples >= 0 THEN c.reltuples::bigint END{rows_parts} END"
                ),
            )
        } else {
            ("NULL::bigint".to_string(), "NULL::bigint".to_string())
        };
        let partition_filter = if partitions {
            "AND NOT c.relispartition"
        } else {
            ""
        };
        let rows = self
            .client
            .query(
                &format!(
                    "SELECT n.nspname::text, c.relname::text, c.relkind::text, {size}, {rows} \
                     FROM pg_catalog.pg_class c \
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') {partition_filter} \
                       AND {system} \
                     ORDER BY 1, 2"
                ),
                &[],
            )
            .await
            .map_err(pg_error)?;
        let tables = rows
            .iter()
            .map(|row| TableInfo {
                schema: row.get(0),
                name: row.get(1),
                view: matches!(row.get::<_, String>(2).as_str(), "v" | "m"),
                size: to_u64(row.get(3)),
                rows: to_u64(row.get(4)),
            })
            .collect();
        Ok(Catalog { schemas, tables })
    }

    /// Базы на сервере, к которым можно подключиться. Размер — только тех,
    /// к которым есть право подключения (иначе сервер его не скажет).
    async fn databases(&self) -> DbResult<Vec<DatabaseInfo>> {
        let rows = match self
            .client
            .query(
                "SELECT datname::text, datname = current_database(), \
                        CASE WHEN pg_catalog.has_database_privilege(datname, 'CONNECT') \
                             THEN pg_catalog.pg_database_size(datname) END \
                 FROM pg_catalog.pg_database \
                 WHERE datallowconn AND NOT datistemplate ORDER BY 1",
                &[],
            )
            .await
        {
            Ok(rows) => rows,
            Err(_) => self
                .client
                .query(
                    "SELECT datname::text, datname = current_database(), NULL::bigint \
                     FROM pg_catalog.pg_database \
                     WHERE datallowconn AND NOT datistemplate ORDER BY 1",
                    &[],
                )
                .await
                .map_err(pg_error)?,
        };
        Ok(rows
            .iter()
            .map(|row| DatabaseInfo {
                name: row.get(0),
                current: row.get(1),
                size: to_u64(row.get(2)),
            })
            .collect())
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

    async fn export(&self, sql: &str, mut writer: Box<dyn RowWriter>) -> DbResult<u64> {
        // Типы колонок — из описания подготовленного запроса (он не
        // выполняется); значения — текстом по простому протоколу.
        let kinds: Vec<ValueKind> = match self.client.prepare(sql).await {
            Ok(statement) => statement
                .columns()
                .iter()
                .map(|c| pg_kind(c.type_()))
                .collect(),
            Err(_) => Vec::new(),
        };
        let stream = self.client.simple_query_raw(sql).await.map_err(pg_error)?;
        let mut stream = std::pin::pin!(stream);
        // Пишется только первый результат с колонками.
        let mut state = 0u8; // 0 — ждём колонки, 1 — пишем, 2 — готово
        let mut count = 0u64;
        while let Some(message) = stream.next().await {
            match message.map_err(pg_error)? {
                SimpleQueryMessage::RowDescription(columns) if state == 0 => {
                    let columns: Vec<ExportColumn> = columns
                        .iter()
                        .enumerate()
                        .map(|(i, c)| ExportColumn {
                            name: c.name().to_string(),
                            kind: kinds.get(i).copied().unwrap_or(ValueKind::Text),
                        })
                        .collect();
                    writer.start(&columns).map_err(write_error)?;
                    state = 1;
                }
                SimpleQueryMessage::Row(row) if state == 1 => {
                    let values: Vec<Option<String>> = (0..row.len())
                        .map(|i| row.get(i).map(str::to_string))
                        .collect();
                    writer.row(&values).map_err(write_error)?;
                    count += 1;
                }
                SimpleQueryMessage::CommandComplete(_) if state == 1 => state = 2,
                _ => {}
            }
        }
        if state == 0 {
            return Err(DbError::new("Запрос не вернул строк — выгружать нечего"));
        }
        writer.finish().map_err(write_error)?;
        Ok(count)
    }

    async fn import(
        &self,
        target: ImportTarget,
        mut chunks: mpsc::Receiver<Chunk>,
        progress: Progress,
    ) -> DbResult<u64> {
        let (table, columns) = target_name(&target, DbKind::Postgres);
        let result = async {
            self.client.batch_execute("BEGIN").await.map_err(pg_error)?;
            if target.truncate {
                self.client
                    .batch_execute(&format!("DELETE FROM {table}"))
                    .await
                    .map_err(pg_error)?;
            }
            let copy = format!("COPY {table} ({columns}) FROM STDIN WITH (FORMAT csv)");
            let mut total = 0u64;
            while let Some(chunk) = chunks.recv().await {
                let rows = chunk.map_err(DbError::new)?;
                let start = total + 1;
                let sent = async {
                    let sink = self
                        .client
                        .copy_in::<_, bytes::Bytes>(&copy)
                        .await
                        .map_err(pg_error)?;
                    let mut sink = std::pin::pin!(sink);
                    sink.send(bytes::Bytes::from(transfer::copy_csv(&rows)))
                        .await
                        .map_err(pg_error)?;
                    sink.as_mut().finish().await.map_err(pg_error)
                }
                .await;
                match sent {
                    Ok(count) => total += count,
                    Err(mut err) => {
                        err.message.push_str(&format!(
                            "\n(ошибка в строках данных {start}–{})",
                            total + rows.len() as u64
                        ));
                        return Err(err);
                    }
                }
                progress(total);
            }
            self.client
                .batch_execute("COMMIT")
                .await
                .map_err(pg_error)?;
            Ok(total)
        }
        .await;
        if result.is_err() {
            let _ = self.client.batch_execute("ROLLBACK").await;
        }
        result
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

/// Значение как текст; двоичные данные для просмотра обрезаются до
/// `limit` байт.
fn my_value_with(value: mysql_async::Value, limit: Option<usize>) -> Option<String> {
    use mysql_async::Value;
    Some(match value {
        Value::NULL => return None,
        Value::Bytes(bytes) => bytes_text(&bytes, limit),
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

fn my_value(value: mysql_async::Value) -> Option<String> {
    my_value_with(value, Some(BLOB_PREVIEW_BYTES))
}

fn my_row(row: mysql_async::Row) -> Vec<Option<String>> {
    row.unwrap().into_iter().map(my_value).collect()
}

fn my_kind(column: &mysql_async::Column) -> ValueKind {
    use mysql_async::consts::ColumnType as T;
    match column.column_type() {
        T::MYSQL_TYPE_TINY
        | T::MYSQL_TYPE_SHORT
        | T::MYSQL_TYPE_LONG
        | T::MYSQL_TYPE_LONGLONG
        | T::MYSQL_TYPE_INT24
        | T::MYSQL_TYPE_FLOAT
        | T::MYSQL_TYPE_DOUBLE
        | T::MYSQL_TYPE_DECIMAL
        | T::MYSQL_TYPE_NEWDECIMAL
        | T::MYSQL_TYPE_YEAR => ValueKind::Number,
        _ => ValueKind::Text,
    }
}

const MYSQL_SYSTEM: &str = "('mysql', 'information_schema', 'performance_schema', 'sys')";

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
        let my = Self::open(opts).await?;
        let version: Option<String> = my
            .conn
            .lock()
            .await
            .query_first("SELECT VERSION()")
            .await
            .ok()
            .flatten();
        let version = match version {
            Some(version) if version.contains("MariaDB") => format!("MariaDB {version}"),
            Some(version) => format!("MySQL {version}"),
            None => "MySQL".to_string(),
        };
        Ok((Backend::MySql(my), version))
    }

    async fn open(opts: mysql_async::Opts) -> DbResult<Self> {
        let conn = mysql_async::Conn::new(opts.clone())
            .await
            .map_err(my_error)?;
        let id = conn.id();
        Ok(Self {
            conn: tokio::sync::Mutex::new(conn),
            opts,
            id,
        })
    }

    async fn reopen(&self) -> DbResult<Backend> {
        Ok(Backend::MySql(Self::open(self.opts.clone()).await?))
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

    /// Строки запроса без параметров как текст.
    async fn text_rows(&self, sql: &str) -> DbResult<Vec<Vec<Option<String>>>> {
        let mut conn = self.conn.lock().await;
        let rows: Vec<mysql_async::Row> = conn.query(sql).await.map_err(my_error)?;
        Ok(rows.into_iter().map(my_row).collect())
    }

    async fn catalog(&self) -> DbResult<Catalog> {
        let schemas = self
            .text_rows(&format!(
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name NOT IN {MYSQL_SYSTEM} ORDER BY 1"
            ))
            .await?
            .into_iter()
            .filter_map(|row| row.into_iter().next().flatten())
            .collect();
        let tables = self
            .text_rows(&format!(
                "SELECT table_schema, table_name, table_type, \
                        data_length + index_length, table_rows \
                 FROM information_schema.tables \
                 WHERE table_schema NOT IN {MYSQL_SYSTEM} ORDER BY 1, 2"
            ))
            .await?
            .into_iter()
            .map(|row| {
                let mut values = row.into_iter();
                let mut next = || values.next().flatten();
                let schema = next().unwrap_or_default();
                let name = next().unwrap_or_default();
                let view = next().unwrap_or_default().contains("VIEW");
                let size = next().and_then(|v| v.parse().ok());
                let rows = next().and_then(|v| v.parse().ok());
                TableInfo {
                    schema,
                    name,
                    view,
                    size: if view { None } else { size },
                    rows: if view { None } else { rows },
                }
            })
            .collect();
        Ok(Catalog { schemas, tables })
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

    async fn export(&self, sql: &str, mut writer: Box<dyn RowWriter>) -> DbResult<u64> {
        let mut conn = self.conn.lock().await;
        let mut result = conn.query_iter(sql).await.map_err(my_error)?;
        let columns: Vec<ExportColumn> = result
            .columns_ref()
            .iter()
            .map(|column| ExportColumn {
                name: column.name_str().into_owned(),
                kind: my_kind(column),
            })
            .collect();
        if columns.is_empty() {
            let _ = result.drop_result().await;
            return Err(DbError::new("Запрос не вернул строк — выгружать нечего"));
        }
        writer.start(&columns).map_err(write_error)?;
        let mut count = 0u64;
        while let Some(row) = result.next().await.map_err(my_error)? {
            let values: Vec<Option<String>> = row
                .unwrap()
                .into_iter()
                .map(|value| my_value_with(value, None))
                .collect();
            writer.row(&values).map_err(write_error)?;
            count += 1;
        }
        // Остальные результаты (если запросов было несколько) не нужны.
        let _ = result.drop_result().await;
        writer.finish().map_err(write_error)?;
        Ok(count)
    }

    async fn import(
        &self,
        target: ImportTarget,
        mut chunks: mpsc::Receiver<Chunk>,
        progress: Progress,
    ) -> DbResult<u64> {
        let (table, columns) = target_name(&target, DbKind::MySql);
        let width = target.columns.len().max(1);
        let placeholders = format!("({})", vec!["?"; width].join(", "));
        let per_insert = (MYSQL_MAX_PARAMS / width).clamp(1, transfer::CHUNK_ROWS);
        let mut conn = self.conn.lock().await;
        let result = async {
            conn.query_drop("START TRANSACTION")
                .await
                .map_err(my_error)?;
            if target.truncate {
                conn.query_drop(format!("DELETE FROM {table}"))
                    .await
                    .map_err(my_error)?;
            }
            let mut total = 0u64;
            while let Some(chunk) = chunks.recv().await {
                let rows = chunk.map_err(DbError::new)?;
                for part in rows.chunks(per_insert) {
                    let statement = format!(
                        "INSERT INTO {table} ({columns}) VALUES {}",
                        vec![placeholders.as_str(); part.len()].join(", ")
                    );
                    let params: Vec<mysql_async::Value> = part
                        .iter()
                        .flat_map(|row| {
                            (0..width).map(|i| match row.get(i).cloned().flatten() {
                                Some(text) => mysql_async::Value::Bytes(text.into_bytes()),
                                None => mysql_async::Value::NULL,
                            })
                        })
                        .collect();
                    if let Err(err) = conn.exec_drop(statement, params).await {
                        let mut err = my_error(err);
                        err.message.push_str(&format!(
                            "\n(ошибка в строках данных {}–{})",
                            total + 1,
                            total + part.len() as u64
                        ));
                        return Err(err);
                    }
                    total += part.len() as u64;
                }
                progress(total);
            }
            conn.query_drop("COMMIT").await.map_err(my_error)?;
            Ok(total)
        }
        .await;
        if result.is_err() {
            let _ = conn.query_drop("ROLLBACK").await;
        }
        result
    }
}

// -------------------------------------------------------------------- SQLite

pub struct Sqlite {
    conn: Arc<Mutex<rusqlite::Connection>>,
    interrupt: Arc<rusqlite::InterruptHandle>,
    path: PathBuf,
}

fn lite_error(err: rusqlite::Error) -> DbError {
    DbError::new(err.to_string())
}

fn lite_value_with(value: rusqlite::types::ValueRef<'_>, limit: Option<usize>) -> Option<String> {
    use rusqlite::types::ValueRef;
    Some(match value {
        ValueRef::Null => return None,
        ValueRef::Integer(v) => v.to_string(),
        ValueRef::Real(v) => v.to_string(),
        ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        // Текст в BLOB-колонке тоже бывает: он показывается как есть.
        ValueRef::Blob(bytes) => bytes_text(bytes, limit),
    })
}

fn lite_value(value: rusqlite::types::ValueRef<'_>) -> Option<String> {
    lite_value_with(value, Some(BLOB_PREVIEW_BYTES))
}

/// Тип колонки по объявленному типу (правила близости типов SQLite).
fn lite_kind(declared: Option<&str>) -> ValueKind {
    let declared = declared.unwrap_or("").to_uppercase();
    if declared.contains("BOOL") {
        ValueKind::Bool
    } else if ["INT", "REAL", "FLOA", "DOUB", "NUMERIC", "DECIMAL"]
        .iter()
        .any(|part| declared.contains(part))
    {
        ValueKind::Number
    } else {
        ValueKind::Text
    }
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

fn lite_catalog(conn: &rusqlite::Connection) -> rusqlite::Result<Catalog> {
    let mut schemas: Vec<String> = Vec::new();
    {
        let mut statement = conn.prepare("SELECT name FROM pragma_database_list ORDER BY seq")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            if name != "temp" {
                schemas.push(name);
            }
        }
    }
    let mut tables = Vec::new();
    for schema in &schemas {
        let quoted = sql::quote_ident(schema, DbKind::Sqlite);
        // Размеры — из виртуальной таблицы dbstat; если SQLite собран без
        // неё, таблицы показываются без размеров.
        let mut sizes = std::collections::HashMap::new();
        if let Ok(mut statement) = conn.prepare(&format!(
            "SELECT m.tbl_name, sum(d.pgsize) FROM dbstat(?1) d \
             JOIN {quoted}.sqlite_master m ON m.name = d.name GROUP BY m.tbl_name"
        )) && let Ok(mut rows) = statement.query([schema])
        {
            while let Ok(Some(row)) = rows.next() {
                if let (Ok(name), Ok(size)) = (row.get::<_, String>(0), row.get::<_, i64>(1)) {
                    sizes.insert(name, u64::try_from(size).unwrap_or(0));
                }
            }
        }
        let mut statement = conn.prepare(&format!(
            "SELECT name, type FROM {quoted}.sqlite_master \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             ORDER BY name"
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let view = row.get::<_, String>(1)? == "view";
            tables.push(TableInfo {
                schema: schema.clone(),
                size: if view {
                    None
                } else {
                    sizes.get(&name).copied()
                },
                name,
                view,
                rows: None,
            });
        }
    }
    Ok(Catalog { schemas, tables })
}

fn lite_export(
    conn: &rusqlite::Connection,
    sql: &str,
    writer: &mut dyn RowWriter,
) -> DbResult<u64> {
    let mut statement = conn.prepare(sql).map_err(lite_error)?;
    if statement.column_count() == 0 {
        return Err(DbError::new("Запрос не вернул строк — выгружать нечего"));
    }
    let columns: Vec<ExportColumn> = statement
        .columns()
        .map(|column| ExportColumn {
            name: column.name().to_string(),
            kind: lite_kind(column.decl_type()),
        })
        .collect();
    writer.start(&columns).map_err(write_error)?;
    let width = columns.len();
    let mut rows = statement.raw_query();
    let mut count = 0u64;
    while let Some(row) = rows.next().map_err(lite_error)? {
        let values: Vec<Option<String>> = (0..width)
            .map(|i| row.get_ref(i).ok().and_then(|v| lite_value_with(v, None)))
            .collect();
        writer.row(&values).map_err(write_error)?;
        count += 1;
    }
    writer.finish().map_err(write_error)?;
    Ok(count)
}

impl Sqlite {
    async fn open(path: &std::path::Path) -> DbResult<(Backend, String)> {
        let path = path.to_path_buf();
        let target = path.clone();
        let conn = tokio::task::spawn_blocking(move || {
            use rusqlite::OpenFlags;
            // Без SQLITE_OPEN_CREATE: опечатка в пути не должна молча
            // создавать пустую базу.
            let conn = rusqlite::Connection::open_with_flags(
                &target,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|err| match err.sqlite_error_code() {
                Some(rusqlite::ErrorCode::CannotOpen) => DbError::new(format!(
                    "Не удалось открыть {}: файла нет или нет доступа",
                    target.display()
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
                path,
            }),
            version,
        ))
    }

    /// Выполняет `job` с соединением в потоке для блокирующих вызовов.
    async fn with_conn<T: Send + 'static>(
        &self,
        job: impl FnOnce(&rusqlite::Connection) -> DbResult<T> + Send + 'static,
    ) -> DbResult<T> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            job(&conn)
        })
        .await
        .map_err(|_| DbError::new("Запрос прервался из-за внутренней ошибки"))?
    }

    async fn query(&self, sql: &str) -> DbResult<QueryOutput> {
        let sql = sql.to_string();
        self.with_conn(move |conn| lite_query(conn, &sql).map_err(lite_error))
            .await
    }

    async fn catalog(&self) -> DbResult<Catalog> {
        self.with_conn(|conn| lite_catalog(conn).map_err(lite_error))
            .await
    }

    async fn structure(&self, schema: &str, table: &str) -> DbResult<QueryOutput> {
        let (schema, table) = (schema.to_string(), table.to_string());
        self.with_conn(move |conn| {
            let mut statement = conn
                .prepare(
                    "SELECT name, type, CASE \"notnull\" WHEN 0 THEN 'YES' ELSE 'NO' END, \
                            dflt_value, CASE pk WHEN 0 THEN '' ELSE 'PK' END \
                     FROM pragma_table_info(?1, ?2)",
                )
                .map_err(lite_error)?;
            let mut output = QueryOutput::table(structure_columns());
            let schema = if schema.is_empty() { "main" } else { &schema };
            let mut rows = statement.query([&table, schema]).map_err(lite_error)?;
            while let Some(row) = rows.next().map_err(lite_error)? {
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

    async fn export(&self, sql: &str, mut writer: Box<dyn RowWriter>) -> DbResult<u64> {
        let sql = sql.to_string();
        self.with_conn(move |conn| lite_export(conn, &sql, writer.as_mut()))
            .await
    }

    async fn import(
        &self,
        target: ImportTarget,
        mut chunks: mpsc::Receiver<Chunk>,
        progress: Progress,
    ) -> DbResult<u64> {
        let (table, columns) = target_name(&target, DbKind::Sqlite);
        let width = target.columns.len();
        self.with_conn(move |conn| {
            let placeholders: Vec<String> = (1..=width).map(|i| format!("?{i}")).collect();
            let insert = format!(
                "INSERT INTO {table} ({columns}) VALUES ({})",
                placeholders.join(", ")
            );
            let mut run = || -> DbResult<u64> {
                conn.execute_batch("BEGIN IMMEDIATE").map_err(lite_error)?;
                if target.truncate {
                    conn.execute(&format!("DELETE FROM {table}"), [])
                        .map_err(lite_error)?;
                }
                let mut statement = conn.prepare(&insert).map_err(lite_error)?;
                let mut total = 0u64;
                while let Some(chunk) = chunks.blocking_recv() {
                    let rows = chunk.map_err(DbError::new)?;
                    for row in &rows {
                        let values = (0..width).map(|i| row.get(i).and_then(Option::as_deref));
                        statement
                            .execute(rusqlite::params_from_iter(values))
                            .map_err(|err| {
                                DbError::new(format!(
                                    "{}\n(ошибка в строке данных {})",
                                    err,
                                    total + 1
                                ))
                            })?;
                        total += 1;
                    }
                    progress(total);
                }
                drop(statement);
                conn.execute_batch("COMMIT").map_err(lite_error)?;
                Ok(total)
            };
            let result = run();
            if result.is_err() {
                let _ = conn.execute_batch("ROLLBACK");
            }
            result
        })
        .await
    }
}

// ---------------------------------------------------------------- расширения

pub struct Extension {
    plugin: Arc<Plugin>,
    info: ExtensionInfo,
}

fn call_error(err: extension::CallError) -> DbError {
    DbError {
        message: err.message,
        lost: err.lost,
    }
}

fn json_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

fn json_str(value: &Value, key: &str) -> String {
    value
        .get(key)
        .map(|v| extension::json_text(v).unwrap_or_default())
        .unwrap_or_default()
}

/// Ответ `query` расширения: колонки, строки, итоги и (необязательно)
/// типы колонок для экспорта.
fn ext_output(value: &Value, limit: Option<usize>) -> (QueryOutput, Vec<ValueKind>) {
    let columns: Vec<String> = value
        .get("columns")
        .and_then(Value::as_array)
        .map(|columns| {
            columns
                .iter()
                .map(|column| match column {
                    Value::Object(_) => json_str(column, "name"),
                    other => extension::json_text(other).unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut output = QueryOutput::table(columns);
    if let Some(rows) = value.get("rows").and_then(Value::as_array) {
        for row in rows {
            let values = match row {
                Value::Array(values) => values.iter().map(extension::json_text).collect(),
                Value::Object(map) => output
                    .columns
                    .iter()
                    .map(|name| map.get(name).and_then(extension::json_text))
                    .collect(),
                other => vec![extension::json_text(other)],
            };
            if limit.is_some_and(|limit| output.rows.len() >= limit) {
                output.truncated = true;
                break;
            }
            output.rows.push(values);
        }
    }
    output.truncated |= value
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    output.affected = json_u64(value, "affected");
    output.statements = json_u64(value, "statements")
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(1);
    let kinds = value
        .get("types")
        .and_then(Value::as_array)
        .map(|types| {
            types
                .iter()
                .map(|t| match t.as_str().unwrap_or("") {
                    "number" => ValueKind::Number,
                    "bool" => ValueKind::Bool,
                    _ => ValueKind::Text,
                })
                .collect()
        })
        .unwrap_or_default();
    (output, kinds)
}

fn ext_catalog(value: &Value) -> Catalog {
    let list = match value {
        Value::Array(_) => Some(value),
        other => other.get("tables"),
    };
    let tables: Vec<TableInfo> = list
        .and_then(Value::as_array)
        .map(|tables| {
            tables
                .iter()
                .map(|table| TableInfo {
                    schema: json_str(table, "schema"),
                    name: json_str(table, "name"),
                    view: table.get("view").and_then(Value::as_bool).unwrap_or(false),
                    size: json_u64(table, "size"),
                    rows: json_u64(table, "rows"),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut schemas: Vec<String> = value
        .get("schemas")
        .and_then(Value::as_array)
        .map(|schemas| schemas.iter().filter_map(extension::json_text).collect())
        .unwrap_or_default();
    for table in &tables {
        if !schemas.contains(&table.schema) {
            schemas.push(table.schema.clone());
        }
    }
    Catalog { schemas, tables }
}

impl Extension {
    async fn connect(
        profile: &DbProfile,
        password: &str,
        tunnel: Option<&Tunnel>,
    ) -> DbResult<(Backend, String)> {
        let info = profile.extension().ok_or_else(|| {
            DbError::new(format!("Расширение «{}» не установлено", profile.driver))
        })?;
        let plugin = Plugin::start(&info).map_err(DbError::new)?;
        let (host, port) = match tunnel.and_then(Tunnel::port) {
            Some(port) => ("127.0.0.1".to_string(), port),
            None => (profile.host.clone(), profile.port),
        };
        let reply = plugin
            .call(
                "connect",
                json!({
                    "host": host,
                    "port": port,
                    "user": profile.user,
                    "password": password,
                    "database": profile.database,
                    "path": profile.path,
                    "tls": profile.tls,
                }),
            )
            .await
            .map_err(call_error)?;
        let version = json_str(&reply, "version");
        let version = if version.is_empty() {
            info.name.clone()
        } else if version.contains(&info.name) {
            version
        } else {
            format!("{} {version}", info.name)
        };
        Ok((
            Backend::Extension(Extension {
                plugin: Arc::new(plugin),
                info,
            }),
            version,
        ))
    }

    async fn call(&self, method: &str, params: Value) -> DbResult<Value> {
        self.plugin.call(method, params).await.map_err(call_error)
    }

    async fn query(&self, sql: &str) -> DbResult<QueryOutput> {
        let reply = self
            .call("query", json!({"sql": sql, "limit": MAX_ROWS}))
            .await?;
        Ok(ext_output(&reply, Some(MAX_ROWS)).0)
    }

    async fn catalog(&self) -> DbResult<Catalog> {
        Ok(ext_catalog(&self.call("tables", json!({})).await?))
    }

    async fn structure(&self, schema: &str, table: &str) -> DbResult<QueryOutput> {
        let reply = self
            .call("structure", json!({"schema": schema, "table": table}))
            .await?;
        let mut output = QueryOutput::table(structure_columns());
        let columns = match &reply {
            Value::Array(_) => Some(&reply),
            other => other.get("columns"),
        };
        for column in columns.and_then(Value::as_array).into_iter().flatten() {
            let nullable = column
                .get("nullable")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let key = column
                .get("key")
                .map(|key| match key {
                    Value::Bool(true) => "PK".to_string(),
                    Value::Bool(false) | Value::Null => String::new(),
                    other => extension::json_text(other).unwrap_or_default(),
                })
                .unwrap_or_default();
            output.push(vec![
                Some(json_str(column, "name")),
                Some(json_str(column, "type")),
                Some(if nullable { "YES" } else { "NO" }.to_string()),
                column.get("default").and_then(extension::json_text),
                Some(key),
            ]);
        }
        output.statements = 1;
        Ok(output)
    }

    async fn export(&self, sql: &str, mut writer: Box<dyn RowWriter>) -> DbResult<u64> {
        let reply = self
            .call("query", json!({"sql": sql, "limit": Value::Null}))
            .await?;
        let (output, kinds) = ext_output(&reply, None);
        if output.columns.is_empty() {
            return Err(DbError::new("Запрос не вернул строк — выгружать нечего"));
        }
        let columns: Vec<ExportColumn> = output
            .columns
            .iter()
            .enumerate()
            .map(|(i, name)| ExportColumn {
                name: name.clone(),
                kind: kinds.get(i).copied().unwrap_or(ValueKind::Text),
            })
            .collect();
        writer.start(&columns).map_err(write_error)?;
        for row in &output.rows {
            writer.row(row).map_err(write_error)?;
        }
        writer.finish().map_err(write_error)?;
        Ok(output.rows.len() as u64)
    }

    async fn import(
        &self,
        target: ImportTarget,
        mut chunks: mpsc::Receiver<Chunk>,
        progress: Progress,
    ) -> DbResult<u64> {
        let mut total = 0u64;
        let mut first = true;
        while let Some(chunk) = chunks.recv().await {
            let rows = chunk.map_err(DbError::new)?;
            let reply = self
                .call(
                    "insert",
                    json!({
                        "schema": target.schema,
                        "table": target.table,
                        "columns": target.columns,
                        "rows": rows,
                        "truncate": first && target.truncate,
                    }),
                )
                .await
                .map_err(|mut err| {
                    err.message.push_str(&format!(
                        "\n(ошибка в строках данных {}–{}; {} {} уже вставлено)",
                        total + 1,
                        total + rows.len() as u64,
                        self.info.name,
                        total
                    ));
                    err
                })?;
            first = false;
            total += json_u64(&reply, "affected").unwrap_or(rows.len() as u64);
            progress(total);
        }
        Ok(total)
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
    fn sqlite_catalog_has_sizes() {
        let conn = sqlite_conn();
        conn.execute_batch("CREATE VIEW v AS SELECT name FROM users")
            .expect("view");
        let catalog = lite_catalog(&conn).expect("catalog");
        assert_eq!(catalog.schemas, ["main"]);
        let names: Vec<_> = catalog
            .tables
            .iter()
            .map(|t| (t.name.as_str(), t.view, t.size.is_some()))
            .collect();
        // dbstat есть в сборке SQLite этой системы.
        assert_eq!(names, [("users", false, true), ("v", true, false)]);
    }

    #[test]
    fn sqlite_export_keeps_whole_blobs() {
        let conn = sqlite_conn();
        let long = "ff".repeat(BLOB_PREVIEW_BYTES + 1);
        conn.execute_batch(&format!(
            "INSERT INTO users (name, pic) VALUES ('blob', x'{long}')"
        ))
        .expect("insert");
        let mut writer = Recorder::default();
        let count = lite_export(&conn, "SELECT id, pic FROM users ORDER BY id", &mut writer)
            .expect("export");
        assert_eq!(count, 3);
        assert_eq!(writer.columns[0].kind, ValueKind::Number);
        assert_eq!(writer.rows[2][1], Some(format!("0x{long}")));
        assert!(writer.finished);
    }

    #[derive(Default)]
    struct Recorder {
        columns: Vec<ExportColumn>,
        rows: Vec<Vec<Option<String>>>,
        finished: bool,
    }

    impl RowWriter for Recorder {
        fn start(&mut self, columns: &[ExportColumn]) -> std::io::Result<()> {
            self.columns = columns.to_vec();
            Ok(())
        }
        fn row(&mut self, values: &[Option<String>]) -> std::io::Result<()> {
            self.rows.push(values.to_vec());
            Ok(())
        }
        fn finish(&mut self) -> std::io::Result<()> {
            self.finished = true;
            Ok(())
        }
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
        assert_eq!(
            bytes_text(&[0xff, 0x00], Some(BLOB_PREVIEW_BYTES)),
            "0xff00"
        );
        let long = vec![0xffu8; BLOB_PREVIEW_BYTES + 10];
        assert!(
            bytes_text(&long, Some(BLOB_PREVIEW_BYTES))
                .ends_with(&format!("… ({} Б)", BLOB_PREVIEW_BYTES + 10))
        );
        assert_eq!(bytes_text(&long, None).len(), 2 + long.len() * 2);
    }

    #[test]
    fn extension_replies() {
        let (output, kinds) = ext_output(
            &json!({
                "columns": ["a", {"name": "b"}],
                "rows": [[1, null], {"a": "x", "b": true}],
                "types": ["number", "bool"],
            }),
            Some(1),
        );
        assert_eq!(output.columns, ["a", "b"]);
        assert_eq!(output.rows, [vec![Some("1".to_string()), None]]);
        assert!(output.truncated);
        assert_eq!(kinds, [ValueKind::Number, ValueKind::Bool]);
        let catalog = ext_catalog(&json!([
            {"schema": "db", "name": "t", "size": 1024, "rows": "5"},
            {"schema": "db", "name": "v", "view": true}
        ]));
        assert_eq!(catalog.schemas, ["db"]);
        assert_eq!(catalog.tables[0].size, Some(1024));
        assert_eq!(catalog.tables[0].rows, Some(5));
        assert!(catalog.tables[1].view);
    }

    #[test]
    fn structure_to_columns() {
        let output = QueryOutput {
            columns: structure_columns(),
            rows: vec![
                vec![
                    Some("id".into()),
                    Some("integer".into()),
                    Some("NO".into()),
                    None,
                    Some("PK".into()),
                ],
                vec![
                    Some("name".into()),
                    Some("text".into()),
                    Some("YES".into()),
                    None,
                    Some(String::new()),
                ],
            ],
            ..QueryOutput::default()
        };
        let columns = columns_from_structure(&output);
        assert!(columns[0].primary && !columns[0].nullable);
        assert!(!columns[1].primary && columns[1].nullable);
    }
}
