//! Импорт и экспорт данных: форматы файлов.
//!
//! Экспорт пишет строки по мере получения (таблица целиком в память не
//! загружается). Импорт читает файл в отдельном потоке и отдаёт строки
//! порциями; вставляет их драйвер.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::profile::DbKind;
use super::sql;

/// Сколько строк вставлять за раз.
pub const CHUNK_ROWS: usize = 1000;

/// Строки одной порции импорта.
pub type Rows = Vec<Vec<Option<String>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    /// CSV, который Excel открывает без мастера: `;`, BOM, CRLF.
    ExcelCsv,
    Json,
    Sql,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 4] = [
        ExportFormat::Csv,
        ExportFormat::ExcelCsv,
        ExportFormat::Json,
        ExportFormat::Sql,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::ExcelCsv => "CSV для Excel (;)",
            ExportFormat::Json => "JSON",
            ExportFormat::Sql => "SQL (INSERT)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv | ExportFormat::ExcelCsv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::Sql => "sql",
        }
    }
}

/// Как записывать значение колонки в JSON и SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Text,
    Number,
    Bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportColumn {
    pub name: String,
    pub kind: ValueKind,
}

/// Получатель строк экспорта.
pub trait RowWriter: Send {
    fn start(&mut self, columns: &[ExportColumn]) -> io::Result<()>;
    fn row(&mut self, values: &[Option<String>]) -> io::Result<()>;
    fn finish(&mut self) -> io::Result<()>;
}

/// Создаёт файл и получателя строк для формата. `table` — имя таблицы для
/// `INSERT INTO` (уже в кавычках).
pub fn create_writer(
    path: &Path,
    format: ExportFormat,
    table: &str,
    kind: DbKind,
) -> io::Result<Box<dyn RowWriter>> {
    let file = BufWriter::new(File::create(path)?);
    Ok(writer(Box::new(file), format, table, kind))
}

pub fn writer(
    out: Box<dyn Write + Send>,
    format: ExportFormat,
    table: &str,
    kind: DbKind,
) -> Box<dyn RowWriter> {
    match format {
        ExportFormat::Csv => Box::new(CsvWriter {
            out,
            delimiter: ',',
            bom: false,
            newline: "\n",
        }),
        ExportFormat::ExcelCsv => Box::new(CsvWriter {
            out,
            delimiter: ';',
            bom: true,
            newline: "\r\n",
        }),
        ExportFormat::Json => Box::new(JsonWriter {
            out,
            columns: Vec::new(),
            rows: 0,
        }),
        ExportFormat::Sql => Box::new(SqlWriter {
            out,
            table: table.to_string(),
            kind,
            header: String::new(),
            columns: Vec::new(),
        }),
    }
}

struct CsvWriter {
    out: Box<dyn Write + Send>,
    delimiter: char,
    bom: bool,
    newline: &'static str,
}

impl CsvWriter {
    /// NULL — пустое поле, пустая строка — `""`: так их можно различить
    /// при обратном импорте.
    fn field(&self, value: Option<&str>) -> String {
        let Some(value) = value else {
            return String::new();
        };
        let quote =
            value.is_empty() || value.contains(self.delimiter) || value.contains(['"', '\n', '\r']);
        if quote {
            format!("\"{}\"", value.replace('"', "\"\""))
        } else {
            value.to_string()
        }
    }

    fn line<'a>(&mut self, values: impl Iterator<Item = Option<&'a str>>) -> io::Result<()> {
        let fields: Vec<String> = values.map(|value| self.field(value)).collect();
        let mut line = fields.join(&self.delimiter.to_string());
        line.push_str(self.newline);
        self.out.write_all(line.as_bytes())
    }
}

impl RowWriter for CsvWriter {
    fn start(&mut self, columns: &[ExportColumn]) -> io::Result<()> {
        if self.bom {
            self.out.write_all("\u{feff}".as_bytes())?;
        }
        self.line(columns.iter().map(|c| Some(c.name.as_str())))
    }

    fn row(&mut self, values: &[Option<String>]) -> io::Result<()> {
        self.line(values.iter().map(Option::as_deref))
    }

    fn finish(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

struct JsonWriter {
    out: Box<dyn Write + Send>,
    columns: Vec<ExportColumn>,
    rows: u64,
}

/// Значение для JSON: числа и логические значения без кавычек, если они
/// действительно такие. Число пишется исходным текстом — без потери
/// точности (`NUMERIC` бывает длиннее, чем вмещает `f64`).
fn json_value(value: Option<&str>, kind: ValueKind) -> String {
    let Some(text) = value else {
        return "null".to_string();
    };
    let string = || Value::String(text.to_string()).to_string();
    match kind {
        ValueKind::Number if is_json_number(text) => text.to_string(),
        ValueKind::Bool => match text {
            "t" | "true" | "TRUE" | "1" => "true".to_string(),
            "f" | "false" | "FALSE" | "0" => "false".to_string(),
            _ => string(),
        },
        _ => string(),
    }
}

/// Число в грамматике JSON (без ведущих нулей: `007` — строка).
fn is_json_number(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let whole = digits.split(['.', 'e', 'E']).next().unwrap_or("");
    sql::is_plain_number(text) && (whole == "0" || !whole.starts_with('0'))
}

impl RowWriter for JsonWriter {
    fn start(&mut self, columns: &[ExportColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        self.out.write_all(b"[")
    }

    fn row(&mut self, values: &[Option<String>]) -> io::Result<()> {
        let fields: Vec<String> = self
            .columns
            .iter()
            .zip(values)
            .map(|(column, value)| {
                format!(
                    "{}:{}",
                    Value::String(column.name.clone()),
                    json_value(value.as_deref(), column.kind)
                )
            })
            .collect();
        let separator = if self.rows == 0 { "\n  " } else { ",\n  " };
        self.rows += 1;
        self.out.write_all(separator.as_bytes())?;
        self.out
            .write_all(format!("{{{}}}", fields.join(",")).as_bytes())
    }

    fn finish(&mut self) -> io::Result<()> {
        self.out
            .write_all(if self.rows == 0 { b"]\n" } else { b"\n]\n" })?;
        self.out.flush()
    }
}

struct SqlWriter {
    out: Box<dyn Write + Send>,
    table: String,
    kind: DbKind,
    /// `INSERT INTO t (колонки) VALUES `.
    header: String,
    columns: Vec<ExportColumn>,
}

/// Значение для `INSERT`.
fn sql_value(value: Option<&str>, kind: ValueKind, dialect: DbKind) -> String {
    let Some(text) = value else {
        return "NULL".to_string();
    };
    match kind {
        ValueKind::Number if sql::is_plain_number(text) => text.to_string(),
        ValueKind::Bool => match text {
            "t" | "true" | "TRUE" => "TRUE".to_string(),
            "f" | "false" | "FALSE" => "FALSE".to_string(),
            _ => sql::string_literal(text, dialect),
        },
        _ => sql::string_literal(text, dialect),
    }
}

impl RowWriter for SqlWriter {
    fn start(&mut self, columns: &[ExportColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        let names: Vec<String> = columns
            .iter()
            .map(|c| sql::quote_ident(&c.name, self.kind))
            .collect();
        self.header = format!("INSERT INTO {} ({}) VALUES ", self.table, names.join(", "));
        Ok(())
    }

    fn row(&mut self, values: &[Option<String>]) -> io::Result<()> {
        let literals: Vec<String> = self
            .columns
            .iter()
            .zip(values)
            .map(|(column, value)| sql_value(value.as_deref(), column.kind, self.kind))
            .collect();
        let line = format!("{}({});\n", self.header, literals.join(", "));
        self.out.write_all(line.as_bytes())
    }

    fn finish(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

// --------------------------------------------------------------------- импорт

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportOptions {
    /// Первая строка CSV — имена колонок.
    pub header: bool,
    /// Пустое поле CSV без кавычек — NULL, а не пустая строка (`""` —
    /// всегда пустая строка).
    pub empty_as_null: bool,
    /// Удалить строки таблицы перед вставкой (в той же транзакции).
    pub truncate: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            header: true,
            empty_as_null: true,
            truncate: false,
        }
    }
}

/// Итог импорта или экспорта.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferReport {
    pub rows: u64,
    pub path: PathBuf,
    /// Колонки файла, которых нет в таблице (при импорте пропущены).
    pub skipped: Vec<String>,
    /// Примечание: «выгружены только первые N строк» и т. п.
    pub note: Option<String>,
}

/// Разделитель CSV по первой строке: чего больше вне кавычек.
fn detect_delimiter(line: &str) -> u8 {
    let mut counts = [(b',', 0usize), (b';', 0), (b'\t', 0), (b'|', 0)];
    let mut quoted = false;
    for byte in line.bytes() {
        if byte == b'"' {
            quoted = !quoted;
        } else if !quoted && let Some(entry) = counts.iter_mut().find(|(d, _)| *d == byte) {
            entry.1 += 1;
        }
    }
    // При равенстве побеждает первый по списку (запятая).
    let mut best = (b',', 0);
    for (delimiter, count) in counts {
        if count > best.1 {
            best = (delimiter, count);
        }
    }
    best.0
}

/// Поле CSV: текст и были ли вокруг него кавычки.
type CsvField = (String, bool);

/// Чтение CSV по RFC 4180: поля в кавычках могут содержать разделитель,
/// кавычки (`""`) и переводы строк. В отличие от готовых библиотек помнит,
/// было ли поле в кавычках: так пустое поле (NULL) отличается от `""`.
struct CsvReader {
    reader: Box<dyn BufRead + Send>,
    delimiter: u8,
}

impl CsvReader {
    /// Следующая запись; пустые строки пропускаются.
    fn next_record(&mut self) -> io::Result<Option<Vec<CsvField>>> {
        loop {
            let mut fields = Vec::new();
            let mut field = Vec::new();
            let mut quoted = false;
            let mut in_quotes = false;
            let mut started = false;
            loop {
                let mut line = Vec::new();
                if self.reader.read_until(b'\n', &mut line)? == 0 {
                    if !started {
                        return Ok(None);
                    }
                    // Файл кончился внутри кавычек — берём что есть.
                    break;
                }
                started = true;
                let mut i = 0;
                while i < line.len() {
                    let byte = line[i];
                    if in_quotes {
                        if byte == b'"' {
                            if line.get(i + 1) == Some(&b'"') {
                                field.push(b'"');
                                i += 1;
                            } else {
                                in_quotes = false;
                            }
                        } else {
                            field.push(byte);
                        }
                    } else if byte == b'"' && field.is_empty() && !quoted {
                        in_quotes = true;
                        quoted = true;
                    } else if byte == self.delimiter {
                        fields.push(Self::finish(&mut field, &mut quoted));
                    } else if byte == b'\n' {
                        break;
                    } else if byte == b'\r' && matches!(line.get(i + 1), Some(b'\n') | None) {
                        // CRLF
                    } else {
                        field.push(byte);
                    }
                    i += 1;
                }
                if !in_quotes {
                    break;
                }
            }
            fields.push(Self::finish(&mut field, &mut quoted));
            let blank = fields.len() == 1 && fields[0].0.is_empty() && !fields[0].1;
            if !blank {
                return Ok(Some(fields));
            }
        }
    }

    fn finish(field: &mut Vec<u8>, quoted: &mut bool) -> CsvField {
        let text = String::from_utf8_lossy(field).into_owned();
        field.clear();
        (text, std::mem::take(quoted))
    }
}

/// Открытый файл импорта: имена колонок (если есть) и чтение строк.
pub struct ImportFile {
    /// Имена колонок из файла; `None` — без заголовка, по порядку.
    pub columns: Option<Vec<String>>,
    source: Source,
    empty_as_null: bool,
    /// Номер прочитанной строки файла (для сообщений об ошибках).
    line: u64,
}

enum Source {
    Csv(CsvReader),
    /// JSON целиком в памяти: массив объектов (или массивов).
    Json(std::vec::IntoIter<Value>),
}

fn is_json(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

impl ImportFile {
    pub fn open(path: &Path, options: ImportOptions) -> Result<Self, String> {
        let describe = |err: io::Error| format!("{}: {err}", path.display());
        if is_json(path) {
            let file = File::open(path).map_err(describe)?;
            let value: Value = serde_json::from_reader(BufReader::new(file))
                .map_err(|err| format!("{}: это не JSON: {err}", path.display()))?;
            let Value::Array(items) = value else {
                return Err("Ожидался JSON-массив: [{\"колонка\": значение, …}, …]".to_string());
            };
            // Колонки — все ключи объектов в порядке появления.
            let mut columns: Vec<String> = Vec::new();
            let mut objects = false;
            for item in &items {
                if let Value::Object(map) = item {
                    objects = true;
                    for key in map.keys() {
                        if !columns.contains(key) {
                            columns.push(key.clone());
                        }
                    }
                }
            }
            return Ok(Self {
                columns: objects.then_some(columns),
                source: Source::Json(items.into_iter()),
                empty_as_null: options.empty_as_null,
                line: 0,
            });
        }

        let file = File::open(path).map_err(describe)?;
        let mut reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(file));
        // BOM в начале (так сохраняет Excel) — не часть первого имени.
        if reader
            .fill_buf()
            .map_err(describe)?
            .starts_with(b"\xef\xbb\xbf")
        {
            reader.consume(3);
        }
        let first_line = {
            let buffer = reader.fill_buf().map_err(describe)?;
            let end = buffer
                .iter()
                .position(|b| *b == b'\n')
                .unwrap_or(buffer.len());
            String::from_utf8_lossy(&buffer[..end]).into_owned()
        };
        let mut csv = CsvReader {
            reader,
            delimiter: detect_delimiter(&first_line),
        };
        let mut line = 0;
        let columns = if options.header {
            let Some(record) = csv.next_record().map_err(describe)? else {
                return Err("Файл пустой".to_string());
            };
            line = 1;
            Some(
                record
                    .into_iter()
                    .map(|(name, _)| name.trim().to_string())
                    .collect(),
            )
        } else {
            None
        };
        Ok(Self {
            columns,
            source: Source::Csv(csv),
            empty_as_null: options.empty_as_null,
            line,
        })
    }

    /// Следующая строка файла по колонкам файла (для JSON-объектов — в
    /// порядке `columns`); `None` — файл кончился. `bool_text` — как
    /// записывать логические значения JSON: `("true", "false")` или
    /// `("1", "0")`.
    pub fn next_row(
        &mut self,
        bool_text: (&str, &str),
    ) -> Result<Option<Vec<Option<String>>>, String> {
        self.line += 1;
        let empty_as_null = self.empty_as_null;
        match &mut self.source {
            Source::Csv(reader) => match reader.next_record() {
                Ok(None) => Ok(None),
                Ok(Some(record)) => Ok(Some(
                    record
                        .into_iter()
                        .map(|(text, quoted)| {
                            (quoted || !empty_as_null || !text.is_empty()).then_some(text)
                        })
                        .collect(),
                )),
                Err(err) => Err(format!("Строка {}: {err}", self.line)),
            },
            Source::Json(items) => {
                let Some(item) = items.next() else {
                    return Ok(None);
                };
                let text = |value: &Value| match value {
                    Value::Null => None,
                    Value::String(text) => Some(text.clone()),
                    Value::Bool(true) => Some(bool_text.0.to_string()),
                    Value::Bool(false) => Some(bool_text.1.to_string()),
                    Value::Number(number) => Some(number.to_string()),
                    other => Some(other.to_string()),
                };
                match (&item, &self.columns) {
                    (Value::Object(map), Some(columns)) => Ok(Some(
                        columns
                            .iter()
                            .map(|column| map.get(column).and_then(text))
                            .collect(),
                    )),
                    (Value::Array(values), None) => Ok(Some(values.iter().map(text).collect())),
                    _ => Err(format!(
                        "Элемент {}: ожидался объект {{\"колонка\": значение}}",
                        self.line
                    )),
                }
            }
        }
    }
}

/// Сопоставление колонок файла с колонками таблицы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMap {
    /// Колонки таблицы, в которые вставляются значения.
    pub target: Vec<String>,
    /// Для каждой из них — номер колонки в файле.
    pub source: Vec<usize>,
    /// Колонки файла, которых нет в таблице.
    pub skipped: Vec<String>,
}

impl ColumnMap {
    /// По именам (без учёта регистра) или, без заголовка, по порядку.
    pub fn build(
        file_columns: Option<&[String]>,
        table_columns: &[String],
        first_row_width: usize,
    ) -> Result<Self, String> {
        match file_columns {
            Some(names) => {
                let mut map = ColumnMap {
                    target: Vec::new(),
                    source: Vec::new(),
                    skipped: Vec::new(),
                };
                for (index, name) in names.iter().enumerate() {
                    let found = table_columns
                        .iter()
                        .find(|column| *column == name)
                        .or_else(|| {
                            table_columns
                                .iter()
                                .find(|column| column.to_lowercase() == name.to_lowercase())
                        });
                    match found {
                        Some(column) if !map.target.contains(column) => {
                            map.target.push(column.clone());
                            map.source.push(index);
                        }
                        _ => map.skipped.push(name.clone()),
                    }
                }
                if map.target.is_empty() {
                    return Err(format!(
                        "Ни одна колонка файла не совпала с колонками таблицы.\n\
                         В файле: {}\nВ таблице: {}",
                        names.join(", "),
                        table_columns.join(", ")
                    ));
                }
                Ok(map)
            }
            None => {
                if first_row_width > table_columns.len() {
                    return Err(format!(
                        "В файле {first_row_width} колонок, а в таблице {}. \
                         Добавьте в файл строку с именами колонок.",
                        table_columns.len()
                    ));
                }
                Ok(ColumnMap {
                    target: table_columns[..first_row_width].to_vec(),
                    source: (0..first_row_width).collect(),
                    skipped: Vec::new(),
                })
            }
        }
    }

    /// Значения строки файла в порядке `target`.
    pub fn apply(&self, row: &[Option<String>]) -> Vec<Option<String>> {
        self.source
            .iter()
            .map(|index| row.get(*index).cloned().flatten())
            .collect()
    }
}

/// Данные для `COPY … FROM STDIN (FORMAT csv)`: NULL — пустое поле, строки
/// всегда в кавычках (так пустая строка не станет NULL).
pub fn copy_csv(rows: &[Vec<Option<String>>]) -> Vec<u8> {
    let mut out = Vec::new();
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            if index > 0 {
                out.push(b',');
            }
            if let Some(value) = value {
                out.push(b'"');
                out.extend_from_slice(value.replace('"', "\"\"").as_bytes());
                out.push(b'"');
            }
        }
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Буфер, который можно отдать писателю и прочитать после.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn export(format: ExportFormat, kind: DbKind) -> String {
        let buffer = Shared::default();
        let mut writer = writer(Box::new(buffer.clone()), format, "\"t\"", kind);
        writer
            .start(&[
                ExportColumn {
                    name: "id".into(),
                    kind: ValueKind::Number,
                },
                ExportColumn {
                    name: "name".into(),
                    kind: ValueKind::Text,
                },
                ExportColumn {
                    name: "ok".into(),
                    kind: ValueKind::Bool,
                },
            ])
            .unwrap();
        writer
            .row(&[
                Some("1".into()),
                Some("Аня; \"А\"".into()),
                Some("t".into()),
            ])
            .unwrap();
        writer
            .row(&[Some("2".into()), Some(String::new()), None])
            .unwrap();
        writer.finish().unwrap();
        String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn csv_export() {
        assert_eq!(
            export(ExportFormat::Csv, DbKind::Postgres),
            "id,name,ok\n1,\"Аня; \"\"А\"\"\",t\n2,\"\",\n"
        );
        assert_eq!(
            export(ExportFormat::ExcelCsv, DbKind::Postgres),
            "\u{feff}id;name;ok\r\n1;\"Аня; \"\"А\"\"\";t\r\n2;\"\";\r\n"
        );
    }

    #[test]
    fn json_export() {
        let text = export(ExportFormat::Json, DbKind::Postgres);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            serde_json::json!([
                {"id": 1, "name": "Аня; \"А\"", "ok": true},
                {"id": 2, "name": "", "ok": null}
            ])
        );
        // Порядок колонок сохраняется.
        assert!(text.contains("{\"id\":1,\"name\""), "{text}");
    }

    #[test]
    fn sql_export() {
        assert_eq!(
            export(ExportFormat::Sql, DbKind::Postgres),
            "INSERT INTO \"t\" (\"id\", \"name\", \"ok\") VALUES (1, 'Аня; \"А\"', TRUE);\n\
             INSERT INTO \"t\" (\"id\", \"name\", \"ok\") VALUES (2, '', NULL);\n"
        );
    }

    #[test]
    fn json_numbers_keep_precision() {
        assert_eq!(
            json_value(Some("12345678901234567890.1234567890"), ValueKind::Number),
            "12345678901234567890.1234567890"
        );
        assert_eq!(json_value(Some("007"), ValueKind::Number), "\"007\"");
        assert_eq!(json_value(Some("0.50"), ValueKind::Number), "0.50");
        assert_eq!(json_value(Some("NaN"), ValueKind::Number), "\"NaN\"");
        assert_eq!(json_value(Some("a\"b"), ValueKind::Text), "\"a\\\"b\"");
    }

    #[test]
    fn delimiters() {
        assert_eq!(detect_delimiter("a,b,c"), b',');
        assert_eq!(detect_delimiter("a;b;\"c,d,e\""), b';');
        assert_eq!(detect_delimiter("a\tb"), b'\t');
        assert_eq!(detect_delimiter("single"), b',');
    }

    fn read_all(path: &Path, options: ImportOptions) -> (Option<Vec<String>>, Rows) {
        let mut file = ImportFile::open(path, options).expect("open");
        let mut rows = Vec::new();
        while let Some(row) = file.next_row(("true", "false")).expect("row") {
            rows.push(row);
        }
        (file.columns.clone(), rows)
    }

    #[test]
    fn csv_import_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.csv");
        std::fs::write(
            &path,
            export(ExportFormat::ExcelCsv, DbKind::Postgres).as_bytes(),
        )
        .unwrap();
        let (columns, rows) = read_all(&path, ImportOptions::default());
        assert_eq!(columns.unwrap(), ["id", "name", "ok"]);
        assert_eq!(
            rows,
            [
                vec![
                    Some("1".into()),
                    Some("Аня; \"А\"".into()),
                    Some("t".into())
                ],
                // `""` — пустая строка, пустое поле — NULL.
                vec![Some("2".into()), Some(String::new()), None],
            ]
        );
        let (_, rows) = read_all(
            &path,
            ImportOptions {
                empty_as_null: false,
                ..ImportOptions::default()
            },
        );
        assert_eq!(
            rows[1],
            [Some("2".into()), Some(String::new()), Some(String::new())]
        );
    }

    #[test]
    fn csv_parser_edge_cases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.csv");
        std::fs::write(
            &path,
            "a,b,c\r\n\"мно\r\nго\",\"x\"\"y\",\r\n\r\n1,,\"\"\n2\nlast,\"unterminated",
        )
        .unwrap();
        let (columns, rows) = read_all(&path, ImportOptions::default());
        assert_eq!(columns.unwrap(), ["a", "b", "c"]);
        assert_eq!(
            rows,
            [
                vec![Some("мно\r\nго".into()), Some("x\"y".into()), None],
                vec![Some("1".into()), None, Some(String::new())],
                vec![Some("2".into())],
                vec![Some("last".into()), Some("unterminated".into())],
            ]
        );
        let (columns, rows) = read_all(
            &path,
            ImportOptions {
                header: false,
                ..ImportOptions::default()
            },
        );
        assert!(columns.is_none());
        assert_eq!(
            rows[0],
            [Some("a".into()), Some("b".into()), Some("c".into())]
        );
    }

    #[test]
    fn json_import() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, r#"[{"a": 1, "b": "x"}, {"b": null, "c": true}]"#).unwrap();
        let (columns, rows) = read_all(&path, ImportOptions::default());
        assert_eq!(columns.unwrap(), ["a", "b", "c"]);
        assert_eq!(
            rows,
            [
                vec![Some("1".into()), Some("x".into()), None],
                vec![None, None, Some("true".into())],
            ]
        );
        std::fs::write(&path, r#"{"a": 1}"#).unwrap();
        assert!(ImportFile::open(&path, ImportOptions::default()).is_err());
    }

    #[test]
    fn column_mapping() {
        let table = vec!["id".to_string(), "Name".to_string(), "price".to_string()];
        let names = vec!["name".to_string(), "extra".to_string(), "id".to_string()];
        let map = ColumnMap::build(Some(&names), &table, 3).unwrap();
        assert_eq!(map.target, ["Name", "id"]);
        assert_eq!(map.skipped, ["extra"]);
        assert_eq!(
            map.apply(&[Some("a".into()), Some("b".into()), Some("1".into())]),
            [Some("a".into()), Some("1".into())]
        );
        assert!(ColumnMap::build(Some(&["zzz".to_string()]), &table, 1).is_err());
        let positional = ColumnMap::build(None, &table, 2).unwrap();
        assert_eq!(positional.target, ["id", "Name"]);
        assert!(ColumnMap::build(None, &table, 4).is_err());
    }

    #[test]
    fn copy_data() {
        assert_eq!(
            copy_csv(&[vec![Some("a\"b".into()), None, Some(String::new())]]),
            b"\"a\"\"b\",,\"\"\n"
        );
    }
}
