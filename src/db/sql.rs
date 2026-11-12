//! Работа с текстом SQL без обращения к серверу: поиск запроса под курсором
//! и экранирование имён.

use std::ops::Range;

use super::profile::DbKind;

/// Позиции `;`, разделяющих запросы: точки с запятой внутри строк, имён в
/// кавычках, комментариев и (для PostgreSQL) `$$`-блоков не считаются.
fn separators(text: &str, kind: DbKind) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b';' => {
                result.push(i);
                i += 1;
            }
            quote @ (b'\'' | b'"' | b'`') => {
                i = skip_quoted(bytes, i, quote, kind == DbKind::MySql && quote != b'`');
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => i = skip_line(bytes, i),
            b'#' if kind == DbKind::MySql => i = skip_line(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = match text[i + 2..].find("*/") {
                    Some(end) => i + 2 + end + 2,
                    None => bytes.len(),
                };
            }
            b'$' if kind == DbKind::Postgres => i = skip_dollar(text, i),
            _ => i += 1,
        }
    }
    result
}

/// Конец строки в кавычках, начинающейся с `start`. Удвоенная кавычка —
/// это кавычка внутри строки; `backslash` — обратная косая экранирует
/// следующий символ (как в MySQL).
fn skip_quoted(bytes: &[u8], start: usize, quote: u8, backslash: bool) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        let byte = bytes[i];
        if backslash && byte == b'\\' {
            i += 2;
            continue;
        }
        if byte == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

fn skip_line(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |offset| start + offset + 1)
}

/// `$tag$ … $tag$` в PostgreSQL. Если это не начало такого блока (например,
/// параметр `$1`), пропускается только сам `$`.
fn skip_dollar(text: &str, start: usize) -> usize {
    let rest = &text[start + 1..];
    let Some(close) = rest.find('$') else {
        return start + 1;
    };
    let tag = &rest[..close];
    let valid_tag = tag
        .chars()
        .enumerate()
        .all(|(n, c)| c == '_' || c.is_alphabetic() || (n > 0 && c.is_ascii_digit()));
    if !valid_tag {
        return start + 1;
    }
    let delimiter = format!("${tag}$");
    let body = start + delimiter.len();
    match text[body..].find(&delimiter) {
        Some(end) => body + end + delimiter.len(),
        None => text.len(),
    }
}

/// Есть ли в куске текста что-то кроме пробелов и комментариев.
fn has_code(text: &str, kind: DbKind) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b if b.is_ascii_whitespace() => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => i = skip_line(bytes, i),
            b'#' if kind == DbKind::MySql => i = skip_line(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = match text[i + 2..].find("*/") {
                    Some(end) => i + 2 + end + 2,
                    None => bytes.len(),
                };
            }
            _ => return true,
        }
    }
    false
}

/// Отрезок без пробелов по краям; `None`, если в нём нет кода.
fn trimmed(text: &str, segment: &Range<usize>, kind: DbKind) -> Option<Range<usize>> {
    let piece = &text[segment.clone()];
    if !has_code(piece, kind) {
        return None;
    }
    let lead = piece.len() - piece.trim_start().len();
    let trail = piece.len() - piece.trim_end().len();
    Some(segment.start + lead..segment.end - trail)
}

/// Байтовый диапазон запроса, в котором стоит курсор (`cursor` — байтовое
/// смещение), без пробелов по краям и без завершающей `;`. Курсор сразу
/// после `;` относится к запросу перед ней. Если под курсором пусто,
/// берётся ближайший запрос выше, а если и там пусто — ниже. `None` — в
/// тексте нет ни одного запроса.
pub fn statement_at(text: &str, cursor: usize, kind: DbKind) -> Option<Range<usize>> {
    let cursor = cursor.min(text.len());
    let mut segments = Vec::new();
    let mut start = 0;
    for sep in separators(text, kind) {
        segments.push(start..sep);
        start = sep + 1;
    }
    segments.push(start..text.len());

    // Отрезок, в котором курсор; позиция сразу после `;` ещё относится к
    // запросу перед ней.
    let last = segments.len() - 1;
    let index = segments
        .iter()
        .enumerate()
        .position(|(n, segment)| cursor <= segment.end + usize::from(n < last))
        .unwrap_or(last);
    segments[..=index]
        .iter()
        .rev()
        .chain(&segments[index + 1..])
        .find_map(|segment| trimmed(text, segment, kind))
}

/// Вид фрагмента для подсветки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Keyword,
    String,
    Number,
    Comment,
}

/// Ключевые слова, которые подсвечиваются (в верхнем регистре).
const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "ANALYZE",
    "AND",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CASE",
    "CAST",
    "CHECK",
    "COLUMN",
    "COMMIT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "DATABASE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FOREIGN",
    "FROM",
    "FULL",
    "GRANT",
    "GROUP",
    "HAVING",
    "IF",
    "ILIKE",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRIMARY",
    "REFERENCES",
    "RENAME",
    "RETURNING",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "SCHEMA",
    "SELECT",
    "SET",
    "SHOW",
    "TABLE",
    "THEN",
    "TO",
    "TRANSACTION",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

fn is_word_byte(byte: u8) -> bool {
    byte == b'_' || byte == b'$' || byte.is_ascii_alphanumeric() || byte >= 0x80
}

/// Фрагменты для подсветки: ключевые слова, строки, числа и комментарии.
/// Имена в кавычках (`"name"`, `` `name` ``) не подсвечиваются.
pub fn highlight(text: &str, kind: DbKind) -> Vec<(Range<usize>, Token)> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        match bytes[i] {
            b'\'' => {
                i = skip_quoted(bytes, i, b'\'', kind == DbKind::MySql);
                tokens.push((start..i, Token::String));
            }
            b'"' if kind == DbKind::MySql => {
                i = skip_quoted(bytes, i, b'"', true);
                tokens.push((start..i, Token::String));
            }
            quote @ (b'"' | b'`') => i = skip_quoted(bytes, i, quote, false),
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = skip_line(bytes, i);
                tokens.push((start..i, Token::Comment));
            }
            b'#' if kind == DbKind::MySql => {
                i = skip_line(bytes, i);
                tokens.push((start..i, Token::Comment));
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = match text[i + 2..].find("*/") {
                    Some(end) => i + 2 + end + 2,
                    None => bytes.len(),
                };
                tokens.push((start..i, Token::Comment));
            }
            b'$' if kind == DbKind::Postgres => {
                i = skip_dollar(text, i);
                if i == start + 1 {
                    // Параметр `$1`: не строка и не число.
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                } else {
                    tokens.push((start..i, Token::String));
                }
            }
            b if b.is_ascii_digit() => {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.') {
                    i += 1;
                }
                tokens.push((start..i, Token::Number));
            }
            b if is_word_byte(b) => {
                while i < bytes.len() && is_word_byte(bytes[i]) {
                    i += 1;
                }
                let word = text[start..i].to_ascii_uppercase();
                if KEYWORDS.binary_search(&word.as_str()).is_ok() {
                    tokens.push((start..i, Token::Keyword));
                }
            }
            _ => i += 1,
        }
    }
    tokens
}

/// Имя таблицы или схемы в кавычках, принятых в этой СУБД.
pub fn quote_ident(name: &str, kind: DbKind) -> String {
    match kind {
        DbKind::MySql => format!("`{}`", name.replace('`', "``")),
        DbKind::Postgres | DbKind::Sqlite => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

/// Полное имя таблицы: `схема.таблица` (для SQLite основная схема
/// опускается).
pub fn qualified(schema: &str, table: &str, kind: DbKind) -> String {
    if schema.is_empty() || (kind == DbKind::Sqlite && schema == "main") {
        quote_ident(table, kind)
    } else {
        format!("{}.{}", quote_ident(schema, kind), quote_ident(table, kind))
    }
}

/// Запрос «показать первые строки таблицы».
pub fn select_preview(schema: &str, table: &str, kind: DbKind, limit: usize) -> String {
    format!(
        "SELECT * FROM {} LIMIT {limit};",
        qualified(schema, table, kind)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, cursor: usize, kind: DbKind) -> Option<&str> {
        statement_at(text, cursor, kind).map(|range| &text[range])
    }

    #[test]
    fn picks_statement_under_cursor() {
        let text = "select 1;\nselect 2;\n\nselect 3";
        assert_eq!(at(text, 0, DbKind::Postgres), Some("select 1"));
        assert_eq!(at(text, 9, DbKind::Postgres), Some("select 1"));
        assert_eq!(at(text, 12, DbKind::Postgres), Some("select 2"));
        assert_eq!(at(text, text.len(), DbKind::Postgres), Some("select 3"));
    }

    #[test]
    fn empty_tail_falls_back_to_previous() {
        let text = "select 1;\n\n";
        assert_eq!(at(text, text.len(), DbKind::Postgres), Some("select 1"));
        assert_eq!(at("  \n", 0, DbKind::Postgres), None);
        assert_eq!(at("-- только комментарий\n", 3, DbKind::Postgres), None);
    }

    #[test]
    fn leading_blank_uses_next() {
        let text = ";\n\n  select 1";
        assert_eq!(at(text, 0, DbKind::Sqlite), Some("select 1"));
    }

    #[test]
    fn semicolons_inside_literals_and_comments_are_ignored() {
        let text = "select 'a;b', \"x;y\" -- c;d\n/* e;f */ from t; select 2";
        assert_eq!(
            at(text, 3, DbKind::Postgres),
            Some("select 'a;b', \"x;y\" -- c;d\n/* e;f */ from t")
        );
        assert_eq!(
            at("select 'it''s;' ; x", 0, DbKind::Sqlite),
            Some("select 'it''s;'")
        );
    }

    #[test]
    fn mysql_specifics() {
        let text = "select 'a\\';b', `c;d` # e;f\n; select 2";
        assert_eq!(
            at(text, 0, DbKind::MySql),
            Some("select 'a\\';b', `c;d` # e;f")
        );
        // В PostgreSQL обратная косая — обычный символ.
        assert_eq!(
            at("select 'a\\'; x", 0, DbKind::Postgres),
            Some("select 'a\\'")
        );
    }

    #[test]
    fn postgres_dollar_quotes() {
        let text = "create function f() returns int as $$ begin; return 1; end $$ language plpgsql; select 1";
        assert_eq!(
            at(text, 5, DbKind::Postgres),
            Some("create function f() returns int as $$ begin; return 1; end $$ language plpgsql")
        );
        let tagged = "do $x$ a; b $x$; select $1";
        assert_eq!(at(tagged, 0, DbKind::Postgres), Some("do $x$ a; b $x$"));
        assert_eq!(
            at(tagged, tagged.len(), DbKind::Postgres),
            Some("select $1")
        );
    }

    #[test]
    fn unicode_text() {
        let text = "select 'привет';\nselect 'мир'";
        assert_eq!(at(text, text.len(), DbKind::Postgres), Some("select 'мир'"));
    }

    #[test]
    fn keywords_are_sorted() {
        assert!(KEYWORDS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn highlighting() {
        let text = "SELECT a1, 'x;y', 42 -- note\nfrom \"select\" where n = 3.5 /* c */";
        let pieces: Vec<(&str, Token)> = highlight(text, DbKind::Postgres)
            .into_iter()
            .map(|(range, token)| (&text[range], token))
            .collect();
        assert_eq!(
            pieces,
            [
                ("SELECT", Token::Keyword),
                ("'x;y'", Token::String),
                ("42", Token::Number),
                ("-- note\n", Token::Comment),
                ("from", Token::Keyword),
                ("where", Token::Keyword),
                ("3.5", Token::Number),
                ("/* c */", Token::Comment),
            ]
        );
        // Параметр $1 — не строка, а `$$ … $$` — строка.
        let pg = highlight("select $1, $$a$$", DbKind::Postgres);
        assert_eq!(pg.last().map(|(_, token)| *token), Some(Token::String));
        assert_eq!(pg.len(), 2);
        // В MySQL двойные кавычки — тоже строка, а # — комментарий.
        let my = highlight("select \"a\" # c", DbKind::MySql);
        assert_eq!(my[1].1, Token::String);
        assert_eq!(my[2].1, Token::Comment);
        // Слова с кириллицей и цифрами не разбиваются на ключевые слова.
        assert!(highlight("selectы from1", DbKind::Sqlite).is_empty());
    }

    #[test]
    fn identifiers() {
        assert_eq!(quote_ident("a\"b", DbKind::Postgres), "\"a\"\"b\"");
        assert_eq!(quote_ident("a`b", DbKind::MySql), "`a``b`");
        assert_eq!(qualified("main", "t", DbKind::Sqlite), "\"t\"");
        assert_eq!(
            qualified("public", "t", DbKind::Postgres),
            "\"public\".\"t\""
        );
        assert_eq!(
            select_preview("shop", "orders", DbKind::MySql, 200),
            "SELECT * FROM `shop`.`orders` LIMIT 200;"
        );
    }
}
