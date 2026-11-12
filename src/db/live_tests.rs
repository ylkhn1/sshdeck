//! Проверки на настоящих серверах. По умолчанию пропускаются; запуск:
//!
//! ```sh
//! docker run -d --rm -e POSTGRES_PASSWORD=testpw -p 127.0.0.1:55432:5432 postgres:17-alpine
//! docker run -d --rm -e MARIADB_ROOT_PASSWORD=testpw -e MARIADB_DATABASE=shop \
//!     -p 127.0.0.1:53306:3306 mariadb:11
//! cargo test -- --ignored live_
//! ```
//!
//! Для проверки туннеля нужен SSH-сервер, пускающий по ключу без пароля:
//! `SSHDECK_TEST_SSH=<псевдоним>` (и при необходимости
//! `SSHDECK_SSH_CONFIG=<конфиг>`); туннель ведёт на `127.0.0.1` того сервера.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::*;

const PASSWORD: &str = "testpw";
const PG_PORT: u16 = 55432;
const MY_PORT: u16 = 53306;

struct Live {
    worker: DbWorker,
    events: async_channel::Receiver<Event>,
    next_id: u64,
}

impl Live {
    fn start(profile: DbProfile, control: Option<PathBuf>) -> (Self, String) {
        let (tx, events) = async_channel::unbounded();
        let worker = DbWorker::start(
            ConnectRequest {
                profile,
                password: PASSWORD.to_string(),
                control,
            },
            tx,
        )
        .expect("start");
        let route = match events.recv_blocking().expect("event") {
            Event::Connected { version, route, .. } => {
                eprintln!("подключено: {version} · {route}");
                route
            }
            Event::ConnectFailed(message) => panic!("не подключилось: {message}"),
            other => panic!("неожиданное событие {other:?}"),
        };
        (
            Self {
                worker,
                events,
                next_id: 1,
            },
            route,
        )
    }

    fn wait(&self, id: u64) -> (Result<QueryOutput, DbError>, Duration) {
        loop {
            match self.events.recv_blocking().expect("event") {
                Event::QueryDone {
                    id: done,
                    result,
                    elapsed,
                } if done == id => return (result, elapsed),
                _ => {}
            }
        }
    }

    fn query(&mut self, sql: &str) -> Result<QueryOutput, DbError> {
        let id = self.next_id;
        self.next_id += 1;
        self.worker.send(Request::Query {
            id,
            database: None,
            sql: sql.to_string(),
        });
        self.wait(id).0
    }

    fn ok(&mut self, sql: &str) -> QueryOutput {
        self.query(sql)
            .unwrap_or_else(|err| panic!("{sql}: {}", err.message))
    }

    fn structure(&mut self, schema: &str, table: &str) -> QueryOutput {
        let id = self.next_id;
        self.next_id += 1;
        self.worker.send(Request::Structure {
            id,
            database: None,
            schema: schema.to_string(),
            table: table.to_string(),
        });
        self.wait(id).0.expect("structure")
    }

    /// Каталог базы (`None` — основной).
    fn catalog(&self, database: Option<&str>) -> Catalog {
        self.worker.send(Request::Catalog {
            database: database.map(str::to_string),
        });
        loop {
            if let Event::Catalog {
                database: from,
                result,
            } = self.events.recv_blocking().expect("event")
                && database.is_none_or(|name| name == from)
            {
                return result.expect("catalog");
            }
        }
    }

    /// Запрос к другой базе сервера.
    fn query_in(&mut self, database: &str, sql: &str) -> QueryOutput {
        let id = self.next_id;
        self.next_id += 1;
        self.worker.send(Request::Query {
            id,
            database: Some(database.to_string()),
            sql: sql.to_string(),
        });
        self.wait(id)
            .0
            .unwrap_or_else(|err| panic!("{sql}: {}", err.message))
    }

    fn transfer(
        &mut self,
        request: impl FnOnce(u64) -> Request,
    ) -> Result<TransferReport, DbError> {
        let id = self.next_id;
        self.next_id += 1;
        self.worker.send(request(id));
        loop {
            match self.events.recv_blocking().expect("event") {
                Event::TransferDone {
                    id: done, result, ..
                } if done == id => return result,
                _ => {}
            }
        }
    }

    fn export(
        &mut self,
        sql: &str,
        path: &std::path::Path,
        format: ExportFormat,
        table: &str,
    ) -> u64 {
        let (sql, path, table) = (sql.to_string(), path.to_path_buf(), table.to_string());
        self.transfer(|id| Request::Export {
            id,
            database: None,
            sql,
            path,
            format,
            table,
        })
        .unwrap_or_else(|err| panic!("export: {}", err.message))
        .rows
    }

    fn import(
        &mut self,
        schema: &str,
        table: &str,
        path: &std::path::Path,
        options: ImportOptions,
    ) -> Result<TransferReport, DbError> {
        let (schema, table, path) = (schema.to_string(), table.to_string(), path.to_path_buf());
        self.transfer(|id| Request::Import {
            id,
            database: None,
            schema,
            table,
            path,
            options,
        })
    }

    /// Долгий запрос прерывается кнопкой «Стоп» быстро.
    fn cancel_works(&mut self, sleep_sql: &str) {
        let id = self.next_id;
        self.next_id += 1;
        self.worker.send(Request::Query {
            id,
            database: None,
            sql: sleep_sql.to_string(),
        });
        std::thread::sleep(Duration::from_millis(700));
        let started = Instant::now();
        self.worker.send(Request::Cancel { id });
        let _ = self.wait(id);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "запрос не прервался: {:?}",
            started.elapsed()
        );
        // Соединение после отмены рабочее.
        assert_eq!(self.ok("SELECT 1").rows, [vec![Some("1".to_string())]]);
    }
}

fn cell(output: &QueryOutput, row: usize, column: usize) -> Option<&str> {
    output.rows[row][column].as_deref()
}

fn pg_profile() -> DbProfile {
    DbProfile {
        host: "127.0.0.1".to_string(),
        port: PG_PORT,
        ..DbProfile::template(DbKind::Postgres)
    }
    .normalized()
}

fn my_profile() -> DbProfile {
    DbProfile {
        host: "127.0.0.1".to_string(),
        port: MY_PORT,
        database: "shop".to_string(),
        ..DbProfile::template(DbKind::MySql)
    }
    .normalized()
}

fn check_postgres(live: &mut Live) {
    live.ok("DROP TABLE IF EXISTS sshdeck_items; DROP SCHEMA IF EXISTS sshdeck_s CASCADE");
    live.ok(
        "CREATE TABLE sshdeck_items (id serial PRIMARY KEY, name text NOT NULL, \
         price numeric(10,2), seen timestamptz DEFAULT '2026-11-12 10:00:00+00', \
         flag boolean, meta jsonb, raw bytea)",
    );
    let insert = live.ok(
        "INSERT INTO sshdeck_items (name, price, flag, meta, raw) VALUES \
         ('Чай; зелёный', 12.5, true, '{\"a\": 1}', '\\x00ff'), ('кофе', NULL, false, NULL, NULL)",
    );
    assert_eq!(insert.affected, Some(2));

    let output = live.ok("SET TIME ZONE 'UTC'; SELECT * FROM sshdeck_items ORDER BY id");
    assert_eq!(
        output.columns,
        ["id", "name", "price", "seen", "flag", "meta", "raw"]
    );
    assert_eq!(cell(&output, 0, 1), Some("Чай; зелёный"));
    assert_eq!(cell(&output, 0, 2), Some("12.50"));
    assert_eq!(cell(&output, 0, 3), Some("2026-11-12 10:00:00+00"));
    assert_eq!(cell(&output, 0, 4), Some("t"));
    assert_eq!(cell(&output, 0, 5), Some("{\"a\": 1}"));
    assert_eq!(cell(&output, 0, 6), Some("\\x00ff"));
    assert_eq!(cell(&output, 1, 2), None);
    assert_eq!(output.statements, 2);

    // Пустой результат — всё равно таблица с колонками.
    let empty = live.ok("SELECT id FROM sshdeck_items WHERE false");
    assert_eq!(empty.columns, ["id"]);
    assert!(empty.rows.is_empty());

    let error = live.query("SELEC 1").expect_err("syntax error");
    assert!(error.message.contains("syntax"), "{}", error.message);
    assert!(!error.lost);
    // После ошибки соединение живо.
    live.ok("SELECT 1");

    let many = live.ok("SELECT g FROM generate_series(1, 5000) g");
    assert_eq!(many.rows.len(), MAX_ROWS);
    assert!(many.truncated);
    // Отброшенный остаток не мешает следующему запросу.
    assert_eq!(live.ok("SELECT 42").rows, [vec![Some("42".to_string())]]);

    let structure = live.structure("public", "sshdeck_items");
    assert_eq!(cell(&structure, 0, 0), Some("id"));
    assert_eq!(cell(&structure, 0, 1), Some("integer"));
    assert_eq!(cell(&structure, 0, 2), Some("NO"));
    assert_eq!(cell(&structure, 0, 4), Some("PK"));
    assert_eq!(cell(&structure, 2, 1), Some("numeric(10,2)"));

    live.ok("CREATE SCHEMA sshdeck_s; CREATE VIEW sshdeck_s.v AS SELECT 1 AS x");
    let catalog = live.catalog(None);
    assert!(catalog.schemas.contains(&"sshdeck_s".to_string()));
    let items = catalog
        .tables
        .iter()
        .find(|t| t.schema == "public" && t.name == "sshdeck_items")
        .expect("sshdeck_items");
    assert!(!items.view);
    // Таблица с данными: размер вместе с индексом больше нуля.
    assert!(items.size.is_some_and(|size| size > 0), "{items:?}");
    let view = catalog
        .tables
        .iter()
        .find(|t| t.schema == "sshdeck_s" && t.name == "v")
        .expect("view");
    assert!(view.view && view.size.is_none());

    check_transfer(live, "public", "\"sshdeck_items\"", DbKind::Postgres);

    live.cancel_works("SELECT pg_sleep(30)");
    live.ok("DROP TABLE sshdeck_items; DROP SCHEMA sshdeck_s CASCADE");
}

/// Экспорт таблицы во все форматы, импорт обратно (CSV и JSON) и откат
/// импорта при ошибке. В таблице две строки, первая колонка — `id`.
fn check_transfer(live: &mut Live, schema: &str, table: &str, kind: DbKind) {
    let dir = tempfile::tempdir().expect("tempdir");
    let select = format!("SELECT id, name, price FROM {table} ORDER BY id");
    for format in ExportFormat::ALL {
        let path = dir.path().join(format!("items.{}", format.extension()));
        assert_eq!(live.export(&select, &path, format, table), 2, "{format:?}");
        let text = std::fs::read_to_string(&path).expect("file");
        assert!(text.contains("Чай; зелёный"), "{format:?}: {text}");
        match format {
            ExportFormat::Json => assert!(text.contains("\"price\":12.50"), "{text}"),
            ExportFormat::Sql => assert!(
                text.contains("12.50, ") || text.contains("12.50)"),
                "{text}"
            ),
            _ => {}
        }
    }
    // SQL-файл выполняется обратно как скрипт.
    let script = std::fs::read_to_string(dir.path().join("items.sql")).expect("sql");
    live.ok(&format!("DELETE FROM {table}"));
    live.ok(&script);
    assert_eq!(
        live.ok(&format!("SELECT count(*) FROM {table}")).rows[0][0].as_deref(),
        Some("2")
    );

    // CSV обратно с очисткой таблицы.
    let csv = dir.path().join("items.csv");
    let report = live
        .import(
            schema,
            "sshdeck_items",
            &csv,
            ImportOptions {
                truncate: true,
                ..ImportOptions::default()
            },
        )
        .unwrap_or_else(|err| panic!("import csv: {}", err.message));
    assert_eq!(report.rows, 2);
    assert_eq!(live.ok(&select).rows[0][1].as_deref(), Some("Чай; зелёный"));

    // Большой файл порциями: 2500 строк, NULL и кавычки.
    let big = dir.path().join("big.csv");
    let mut text = String::from("name;price;extra\n");
    for n in 0..2500 {
        text.push_str(&format!("\"имя \"\"{n}\"\"\";{n}.5;x\n"));
    }
    text.push_str("последняя;;x\n");
    std::fs::write(&big, text).unwrap();
    let report = live
        .import(schema, "sshdeck_items", &big, ImportOptions::default())
        .unwrap_or_else(|err| panic!("import big: {}", err.message));
    assert_eq!(report.rows, 2501);
    assert_eq!(report.skipped, ["extra"]);
    let count = live.ok(&format!("SELECT count(*), count(price) FROM {table}"));
    assert_eq!(count.rows[0][0].as_deref(), Some("2503"));
    assert_eq!(count.rows[0][1].as_deref(), Some("2501"));

    // Ошибка в середине (NOT NULL) откатывает весь импорт.
    let broken = dir.path().join("broken.json");
    std::fs::write(
        &broken,
        r#"[{"name": "ок", "price": 1}, {"name": null, "price": 2}]"#,
    )
    .unwrap();
    let error = live
        .import(schema, "sshdeck_items", &broken, ImportOptions::default())
        .expect_err("NOT NULL");
    assert!(error.message.contains("строк"), "{}", error.message);
    let after = live.ok(&format!("SELECT count(*) FROM {table}"));
    assert_eq!(
        after.rows[0][0].as_deref(),
        Some("2503"),
        "импорт не откатился"
    );
    let _ = kind;
}

fn check_mysql(live: &mut Live) {
    live.ok("DROP TABLE IF EXISTS sshdeck_items");
    live.ok(
        "CREATE TABLE sshdeck_items (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(50) NOT NULL, \
         price DECIMAL(10,2), seen DATETIME DEFAULT '2026-11-12 10:00:00', flag BOOL, raw BLOB)",
    );
    let insert = live.ok("INSERT INTO sshdeck_items (name, price, flag, raw) VALUES \
         ('Чай; зелёный', 12.5, 1, x'00ff'), ('кофе', NULL, 0, NULL)");
    assert_eq!(insert.affected, Some(2));

    let output = live.ok("SELECT 1; SELECT * FROM sshdeck_items ORDER BY id");
    assert_eq!(
        output.columns,
        ["id", "name", "price", "seen", "flag", "raw"]
    );
    assert_eq!(cell(&output, 0, 1), Some("Чай; зелёный"));
    assert_eq!(cell(&output, 0, 2), Some("12.50"));
    assert_eq!(cell(&output, 0, 3), Some("2026-11-12 10:00:00"));
    assert_eq!(cell(&output, 0, 4), Some("1"));
    assert_eq!(cell(&output, 0, 5), Some("0x00ff"));
    assert_eq!(cell(&output, 1, 2), None);
    assert_eq!(output.statements, 2);

    let error = live.query("SELEC 1").expect_err("syntax error");
    assert!(error.message.contains("1064"), "{}", error.message);
    live.ok("SELECT 1");

    let many = live.ok("SELECT seq FROM seq_1_to_5000");
    assert_eq!(many.rows.len(), MAX_ROWS);
    assert!(many.truncated);
    assert_eq!(live.ok("SELECT 42").rows, [vec![Some("42".to_string())]]);

    let structure = live.structure("shop", "sshdeck_items");
    assert_eq!(cell(&structure, 0, 0), Some("id"));
    assert_eq!(cell(&structure, 0, 1), Some("int(11)"));
    assert_eq!(cell(&structure, 0, 4), Some("PRI"));

    let catalog = live.catalog(None);
    assert!(catalog.schemas.contains(&"shop".to_string()));
    let items = catalog
        .tables
        .iter()
        .find(|t| t.schema == "shop" && t.name == "sshdeck_items")
        .expect("sshdeck_items");
    assert!(items.size.is_some(), "{items:?}");

    check_transfer(live, "shop", "`shop`.`sshdeck_items`", DbKind::MySql);

    live.cancel_works("SELECT SLEEP(30)");
    live.ok("DROP TABLE sshdeck_items");
}

#[test]
#[ignore = "нужен PostgreSQL на 127.0.0.1:55432"]
fn live_postgres_direct() {
    let (mut live, route) = Live::start(pg_profile(), None);
    assert_eq!(route, "напрямую");
    check_postgres(&mut live);
}

#[test]
#[ignore = "нужен MariaDB/MySQL на 127.0.0.1:53306"]
fn live_mysql_direct() {
    let (mut live, _) = Live::start(my_profile(), None);
    check_mysql(&mut live);
}

#[test]
#[ignore = "нужен PostgreSQL на 127.0.0.1:55432"]
fn live_postgres_wrong_password() {
    let (tx, events) = async_channel::unbounded();
    let _worker = DbWorker::start(
        ConnectRequest {
            profile: pg_profile(),
            password: "wrong".to_string(),
            control: None,
        },
        tx,
    )
    .expect("start");
    match events.recv_blocking().expect("event") {
        Event::ConnectFailed(message) => {
            assert!(message.contains("password"), "{message}");
        }
        other => panic!("ожидалась ошибка: {other:?}"),
    }
}

fn ssh_alias() -> String {
    std::env::var("SSHDECK_TEST_SSH").expect("SSHDECK_TEST_SSH")
}

#[test]
#[ignore = "нужны SSHDECK_TEST_SSH и PostgreSQL за ним"]
fn live_postgres_tunnel() {
    let profile = DbProfile {
        ssh: ssh_alias(),
        ..pg_profile()
    };
    let (mut live, route) = Live::start(profile, None);
    assert!(route.starts_with("через SSH"), "{route}");
    check_postgres(&mut live);
}

#[test]
#[ignore = "нужны SSHDECK_TEST_SSH и MariaDB за ним"]
fn live_mysql_tunnel() {
    let profile = DbProfile {
        ssh: ssh_alias(),
        ..my_profile()
    };
    let (mut live, route) = Live::start(profile, None);
    assert!(route.starts_with("через SSH"), "{route}");
    check_mysql(&mut live);
}

/// Туннель через мастер-соединение «вкладки»; после закрытия базы проброс
/// снимается, а сам мастер остаётся жить.
#[test]
#[ignore = "нужны SSHDECK_TEST_SSH и PostgreSQL за ним"]
fn live_tunnel_through_tab_master() {
    let alias = ssh_alias();
    let runtime = crate::session::runtime_dir().expect("runtime dir");
    let session = crate::session::Session::new(
        &alias,
        &runtime,
        9000 + std::process::id() % 1000,
        crate::session::config_override(),
    );
    // Мастер, как у вкладки терминала, только без tty.
    let mut argv = session.terminal_argv();
    let target = argv.pop().expect("target");
    argv.pop(); // "--"
    argv.extend(["-N".to_string(), "--".to_string(), target]);
    let mut master = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("master");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !session.control_path().exists() {
        assert!(Instant::now() < deadline, "мастер не поднялся");
        std::thread::sleep(Duration::from_millis(100));
    }

    let profile = DbProfile {
        ssh: alias,
        ..pg_profile()
    };
    {
        let (mut live, route) = Live::start(profile, Some(session.control_path().to_path_buf()));
        assert!(route.starts_with("через вкладку"), "{route}");
        assert_eq!(live.ok("SELECT 7").rows, [vec![Some("7".to_string())]]);
    }
    // Воркер закрыт: проброс отменяется в фоне.
    std::thread::sleep(Duration::from_millis(500));
    let check_argv = session.check_argv();
    let check = std::process::Command::new(&check_argv[0])
        .args(&check_argv[1..])
        .output()
        .expect("check");
    assert!(check.status.success(), "мастер должен остаться живым");
    let prefix = format!("db-{}-", std::process::id());
    let leftovers = std::fs::read_dir(&runtime)
        .expect("read runtime")
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .count();
    assert_eq!(leftovers, 0, "каталог туннеля не удалён");

    session.close_master();
    let _ = master.wait();
}

/// Пользователь без прав на таблицы всё равно видит их в навигаторе
/// (раньше список брался из `information_schema` и был пуст), а также
/// получает список баз для переключателя.
#[test]
#[ignore = "нужен PostgreSQL на 127.0.0.1:55432"]
fn live_postgres_tables_without_privileges() {
    let (mut admin, _) = Live::start(pg_profile(), None);
    admin.ok(
        "DROP TABLE IF EXISTS sshdeck_secret; DROP MATERIALIZED VIEW IF EXISTS sshdeck_mv; \
         DROP ROLE IF EXISTS sshdeck_reader; \
         CREATE ROLE sshdeck_reader LOGIN PASSWORD 'testpw'; \
         CREATE TABLE sshdeck_secret (id int); \
         CREATE MATERIALIZED VIEW sshdeck_mv AS SELECT 1 AS x",
    );
    {
        let (tx, events) = async_channel::unbounded();
        let _worker = DbWorker::start(
            ConnectRequest {
                profile: DbProfile {
                    user: "sshdeck_reader".to_string(),
                    database: "postgres".to_string(),
                    ..pg_profile()
                },
                password: PASSWORD.to_string(),
                control: None,
            },
            tx,
        )
        .expect("start");
        let mut tables = None;
        let mut databases = None;
        while tables.is_none() || databases.is_none() {
            match events.recv_blocking().expect("event") {
                Event::Catalog { result, .. } => tables = Some(result.expect("tables").tables),
                Event::Databases(list) => databases = Some(list),
                Event::ConnectFailed(message) => panic!("{message}"),
                _ => {}
            }
        }
        let tables = tables.unwrap_or_default();
        // Размер виден и без прав на саму таблицу.
        assert!(
            tables
                .iter()
                .any(|t| t.name == "sshdeck_secret" && !t.view && t.size.is_some())
        );
        assert!(tables.iter().any(|t| t.name == "sshdeck_mv" && t.view));
        let databases = databases.unwrap_or_default();
        let current = databases.iter().find(|d| d.current).expect("current");
        assert_eq!(current.name, "postgres");
        assert!(current.size.is_some(), "{databases:?}");
    }
    admin.ok(
        "DROP TABLE sshdeck_secret; DROP MATERIALIZED VIEW sshdeck_mv; DROP ROLE sshdeck_reader",
    );
}

/// Дерево из нескольких баз: каталог и запросы к другой базе сервера идут
/// через отдельные соединения, основное соединение остаётся в своей базе.
#[test]
#[ignore = "нужен PostgreSQL на 127.0.0.1:55432"]
fn live_postgres_other_database() {
    let (mut admin, _) = Live::start(pg_profile(), None);
    admin.ok("DROP DATABASE IF EXISTS sshdeck_other");
    admin.ok("CREATE DATABASE sshdeck_other");
    {
        let (mut live, _) = Live::start(pg_profile(), None);
        live.query_in(
            "sshdeck_other",
            "CREATE SCHEMA crm; CREATE TABLE crm.clients (id int PRIMARY KEY, name text); \
             INSERT INTO crm.clients SELECT g, 'клиент ' || g FROM generate_series(1, 300) g; \
             ANALYZE crm.clients",
        );
        let catalog = live.catalog(Some("sshdeck_other"));
        assert!(catalog.schemas.contains(&"crm".to_string()), "{catalog:?}");
        let clients = catalog
            .tables
            .iter()
            .find(|t| t.name == "clients")
            .expect("clients");
        assert_eq!(clients.rows, Some(300));
        assert!(clients.size.is_some_and(|s| s > 8192));
        let current = live.query_in("sshdeck_other", "SELECT current_database()");
        assert_eq!(current.rows[0][0].as_deref(), Some("sshdeck_other"));
        // Основное соединение там же, где было.
        let main = live.ok("SELECT current_database()");
        assert_eq!(main.rows[0][0].as_deref(), Some("postgres"));
    }
    // Соединения вкладки закрылись — базу можно удалить.
    std::thread::sleep(Duration::from_millis(300));
    admin.ok("DROP DATABASE sshdeck_other");
}

/// Расширение из `extensions/clickhouse` на настоящем ClickHouse:
/// `docker run -d --rm -e CLICKHOUSE_PASSWORD=testpw -p 127.0.0.1:58123:8123
/// clickhouse/clickhouse-server:25.8-alpine`.
#[test]
#[ignore = "нужен ClickHouse на 127.0.0.1:58123"]
fn live_clickhouse_extension() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("extensions/clickhouse/sshdeck-driver-clickhouse");
    let output = std::process::Command::new(&script)
        .arg("--sshdeck-info")
        .output()
        .expect("info");
    let mut info = extension::scan_info_for_test(&String::from_utf8_lossy(&output.stdout), &script);
    info.path = script;
    extension::register(info);
    let profile = DbProfile {
        host: "127.0.0.1".to_string(),
        port: 58123,
        ..DbProfile::template_for(DbKind::Extension, "clickhouse".to_string())
    }
    .normalized();
    assert_eq!(profile.user, "default");
    let (mut live, _) = Live::start(profile, None);
    live.ok("DROP TABLE IF EXISTS default.sshdeck_items");
    live.ok(
        "CREATE TABLE default.sshdeck_items (id UInt32, name String, price Nullable(Decimal(10,2))) \
         ENGINE = MergeTree ORDER BY id",
    );
    live.ok(
        "INSERT INTO default.sshdeck_items VALUES (1, 'Чай; зелёный', 12.5), (2, 'кофе', NULL)",
    );
    let rows = live.ok("SELECT * FROM default.sshdeck_items ORDER BY id; SELECT 1 AS one");
    assert_eq!(rows.columns, ["one"]);
    assert_eq!(rows.statements, 2);
    let rows = live.ok("SELECT * FROM default.sshdeck_items ORDER BY id");
    assert_eq!(rows.rows[0][1].as_deref(), Some("Чай; зелёный"));
    assert_eq!(rows.rows[1][2], None);
    let many = live.ok("SELECT number FROM numbers(5000)");
    assert_eq!(many.rows.len(), MAX_ROWS);
    assert!(many.truncated);
    let error = live.query("SELEC 1").expect_err("syntax");
    assert!(error.message.contains("Syntax error"), "{}", error.message);

    let catalog = live.catalog(None);
    let items = catalog
        .tables
        .iter()
        .find(|t| t.schema == "default" && t.name == "sshdeck_items")
        .expect("table");
    assert_eq!(items.rows, Some(2));
    assert!(items.size.is_some_and(|s| s > 0));
    let structure = live.structure("default", "sshdeck_items");
    assert_eq!(cell(&structure, 0, 0), Some("id"));
    assert_eq!(cell(&structure, 0, 4), Some("PK"));
    assert_eq!(cell(&structure, 2, 2), Some("YES"));

    let dir = tempfile::tempdir().expect("tempdir");
    let json = dir.path().join("items.json");
    let exported = live.export(
        "SELECT * FROM default.sshdeck_items ORDER BY id",
        &json,
        ExportFormat::Json,
        "`default`.`sshdeck_items`",
    );
    assert_eq!(exported, 2);
    let text = std::fs::read_to_string(&json).unwrap();
    assert!(text.contains("\"id\":1,"), "{text}");
    assert!(text.contains("\"price\":12.50"), "{text}");

    let csv = dir.path().join("more.csv");
    std::fs::write(&csv, "id,name,price\n10,\"таб\tи\nперевод\",\n11,x,3.25\n").unwrap();
    let report = live
        .import(
            "default",
            "sshdeck_items",
            &csv,
            ImportOptions {
                truncate: true,
                ..ImportOptions::default()
            },
        )
        .unwrap_or_else(|err| panic!("import: {}", err.message));
    assert_eq!(report.rows, 2);
    let rows = live.ok("SELECT id, name, price FROM default.sshdeck_items ORDER BY id");
    assert_eq!(
        rows.rows,
        [
            vec![Some("10".into()), Some("таб\tи\nперевод".into()), None],
            vec![Some("11".into()), Some("x".into()), Some("3.25".into())],
        ]
    );
    live.cancel_works("SELECT sum(number) FROM numbers(100000000000)");
    live.ok("DROP TABLE default.sshdeck_items");
}
