//! Interface translations.
//!
//! Source strings in the code are English. Russian and Spanish live in
//! gettext catalogs (`po/ru.po`, `po/es.po`) that are embedded into the
//! binary, so nothing has to be installed next to it.
//!
//! - `tr!("Text")` translates a string; `tr!("Hello, {}", name)` also fills
//!   in placeholders. `{}` takes arguments in order, `{0}`, `{1}` pick one by
//!   position (a translation may reorder them), `{{` and `}}` are literal
//!   braces.
//! - `trn!("{} row", "{} rows", n)` picks the plural form for `n` (Russian
//!   has three). Without extra arguments `n` fills the placeholders;
//!   otherwise the arguments after `n` do.
//! - `trc!("context", "Text")` is for the same English text meaning
//!   different things (`msgctxt` in the catalog).
//! - `n_!("Text")` only marks a string for the catalog, for `const` tables;
//!   translate it where it is shown with [`tr`].
//!
//! The language is chosen once at start-up ([`init`]); changing it in the
//! menu takes effect after a restart.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;

/// Languages offered in the menu: setting value and name in that language.
/// `system` is shown translated; the others always in their own language.
pub const LANGUAGES: [(&str, &str); 4] = [
    ("system", "System default"),
    ("en", "English"),
    ("ru", "Русский"),
    ("es", "Español"),
];

const RU_PO: &str = include_str!("../po/ru.po");
const ES_PO: &str = include_str!("../po/es.po");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Ru,
    Es,
}

impl Lang {
    fn from_code(code: &str) -> Option<Self> {
        let code = code.trim().to_ascii_lowercase();
        let primary = code.split(['_', '-', '.', '@']).next().unwrap_or("");
        match primary {
            "en" => Some(Self::En),
            "ru" => Some(Self::Ru),
            "es" => Some(Self::Es),
            _ => None,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ru => "ru",
            Self::Es => "es",
        }
    }
}

#[derive(Default)]
struct Catalog {
    /// `msgid` (with `msgctxt` + `\u{4}` in front, if any) → `msgstr`.
    singular: HashMap<String, String>,
    /// Singular `msgid` → plural forms.
    plural: HashMap<String, Vec<String>>,
}

struct State {
    lang: Lang,
    catalog: Catalog,
}

static STATE: OnceLock<State> = OnceLock::new();

/// Chooses the interface language: `choice` is the setting (`system`, `en`,
/// `ru`, `es`). Call once, at the very start of `main`, before any threads
/// exist: for an explicit choice it also sets `LANGUAGE`, so that GTK and
/// libadwaita show their own texts (file chooser, standard buttons) in the
/// same language.
pub fn init(choice: &str) {
    let lang = match Lang::from_code(choice) {
        Some(lang) => {
            // SAFETY: called first thing in `main`, while the process is
            // still single-threaded; nothing reads the environment
            // concurrently.
            unsafe { std::env::set_var("LANGUAGE", lang.code()) };
            lang
        }
        None => system_language(),
    };
    let catalog = match lang {
        Lang::En => Catalog::default(),
        Lang::Ru => parse_po(RU_PO),
        Lang::Es => parse_po(ES_PO),
    };
    let _ = STATE.set(State { lang, catalog });
}

/// Current language; English until [`init`] runs (for example, in tests).
pub fn lang() -> Lang {
    STATE.get().map_or(Lang::En, |state| state.lang)
}

/// Decimal separator of the current language: `1.5 KB` / `1,5 КБ`.
pub fn decimal_separator() -> char {
    match lang() {
        Lang::En => '.',
        Lang::Ru | Lang::Es => ',',
    }
}

/// Language of the desktop session, by the usual gettext variables:
/// the first supported entry of `LANGUAGE`, then `LC_ALL`, `LC_MESSAGES`,
/// `LANG`. Anything unsupported falls back to English.
fn system_language() -> Lang {
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let locale = var("LC_ALL")
        .or_else(|| var("LC_MESSAGES"))
        .or_else(|| var("LANG"));
    // gettext ignores LANGUAGE when the locale is C/POSIX.
    let plain_c = locale
        .as_deref()
        .is_none_or(|value| value == "C" || value.starts_with("C.") || value == "POSIX");
    if !plain_c
        && let Some(list) = var("LANGUAGE")
        && let Some(lang) = list.split(':').find_map(Lang::from_code)
    {
        return lang;
    }
    locale
        .as_deref()
        .and_then(Lang::from_code)
        .unwrap_or(Lang::En)
}

fn catalog() -> Option<&'static Catalog> {
    STATE.get().map(|state| &state.catalog)
}

/// Translation of `msgid`; the English text itself if there is none.
pub fn tr(msgid: &'static str) -> &'static str {
    catalog()
        .and_then(|catalog| catalog.singular.get(msgid))
        .map_or(msgid, String::as_str)
}

/// Translation of `msgid` in context `context` (`msgctxt`).
pub fn trc(context: &'static str, msgid: &'static str) -> &'static str {
    catalog()
        .and_then(|catalog| catalog.singular.get(&context_key(context, msgid)))
        .map_or(msgid, String::as_str)
}

/// Plural form of a translation for `n`.
pub fn trn(singular: &'static str, plural: &'static str, n: u64) -> &'static str {
    let index = plural_index(lang(), n);
    if let Some(form) = catalog()
        .and_then(|catalog| catalog.plural.get(singular))
        .and_then(|forms| forms.get(index))
    {
        return form;
    }
    if n == 1 { singular } else { plural }
}

fn context_key(context: &str, msgid: &str) -> String {
    format!("{context}\u{4}{msgid}")
}

/// Index of the plural form, as in the catalogs' `Plural-Forms` headers.
fn plural_index(lang: Lang, n: u64) -> usize {
    match lang {
        Lang::Ru => {
            if n % 10 == 1 && n % 100 != 11 {
                0
            } else if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) {
                1
            } else {
                2
            }
        }
        Lang::En | Lang::Es => usize::from(n != 1),
    }
}

/// Fills placeholders in a (translated) template; see the module comment.
/// A placeholder without an argument is left as is.
pub fn format(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut next = 0;
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut index = String::new();
                while let Some(&digit) = chars.peek().filter(|c| c.is_ascii_digit()) {
                    index.push(digit);
                    chars.next();
                }
                if chars.peek() != Some(&'}') {
                    out.push('{');
                    out.push_str(&index);
                    continue;
                }
                chars.next();
                let position = if index.is_empty() {
                    next += 1;
                    next - 1
                } else {
                    index.parse().unwrap_or(usize::MAX)
                };
                match args.get(position) {
                    Some(arg) => out.push_str(&arg.to_string()),
                    None => {
                        out.push('{');
                        out.push_str(&index);
                        out.push('}');
                    }
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

/// `tr!("Text")` → `&'static str`; `tr!("Text {}", arg, …)` → `String`.
macro_rules! tr {
    ($msgid:literal) => {
        $crate::i18n::tr($msgid)
    };
    ($msgid:literal, $($arg:expr),+ $(,)?) => {
        $crate::i18n::format(
            $crate::i18n::tr($msgid),
            &[$(&$arg as &dyn ::std::fmt::Display),+],
        )
    };
}

/// `trn!("{} row", "{} rows", n)` or `trn!("…", "…", n, arg, …)` → `String`.
macro_rules! trn {
    ($singular:literal, $plural:literal, $n:expr) => {{
        let n = $n;
        $crate::i18n::format(
            $crate::i18n::trn($singular, $plural, n as u64),
            &[&n as &dyn ::std::fmt::Display],
        )
    }};
    ($singular:literal, $plural:literal, $n:expr, $($arg:expr),+ $(,)?) => {
        $crate::i18n::format(
            $crate::i18n::trn($singular, $plural, ($n) as u64),
            &[$(&$arg as &dyn ::std::fmt::Display),+],
        )
    };
}

/// `trc!("context", "Text")` → `&'static str`; with arguments → `String`.
macro_rules! trc {
    ($context:literal, $msgid:literal) => {
        $crate::i18n::trc($context, $msgid)
    };
    ($context:literal, $msgid:literal, $($arg:expr),+ $(,)?) => {
        $crate::i18n::format(
            $crate::i18n::trc($context, $msgid),
            &[$(&$arg as &dyn ::std::fmt::Display),+],
        )
    };
}

/// Marks a string for the catalog without translating it (for `const`s).
macro_rules! n_ {
    ($msgid:literal) => {
        $msgid
    };
}

/// One catalog entry as written in a `.po` file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PoEntry {
    context: Option<String>,
    msgid: String,
    msgid_plural: Option<String>,
    msgstr: Vec<String>,
    fuzzy: bool,
}

fn parse_po(data: &str) -> Catalog {
    let mut catalog = Catalog::default();
    for entry in po_entries(data) {
        if entry.msgid.is_empty() || entry.fuzzy || entry.msgstr.iter().all(String::is_empty) {
            continue;
        }
        if entry.msgid_plural.is_some() {
            catalog.plural.insert(entry.msgid, entry.msgstr);
        } else {
            let key = match &entry.context {
                Some(context) => context_key(context, &entry.msgid),
                None => entry.msgid,
            };
            if let Some(text) = entry.msgstr.into_iter().next() {
                catalog.singular.insert(key, text);
            }
        }
    }
    catalog
}

/// Minimal `.po` reader: `msgctxt`, `msgid`, `msgid_plural`, `msgstr`,
/// `msgstr[N]`, continuation lines and the `#, fuzzy` flag.
fn po_entries(data: &str) -> Vec<PoEntry> {
    #[derive(Clone, Copy)]
    enum Field {
        None,
        Context,
        Id,
        Plural,
        Str(usize),
    }
    let mut entries = Vec::new();
    let mut entry = PoEntry::default();
    let mut started = false;
    let mut field = Field::None;
    let mut fuzzy_next = false;
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            if comment.starts_with(',') && comment.contains("fuzzy") {
                fuzzy_next = true;
            }
            continue;
        }
        let (keyword, rest) = match line.find(char::is_whitespace) {
            Some(at) if !line.starts_with('"') => (&line[..at], line[at..].trim()),
            _ => ("", line),
        };
        let value = unquote(rest);
        let new_field = match keyword {
            "msgctxt" => Some(Field::Context),
            "msgid" => Some(Field::Id),
            "msgid_plural" => Some(Field::Plural),
            "msgstr" => Some(Field::Str(0)),
            _ => keyword
                .strip_prefix("msgstr[")
                .and_then(|tail| tail.strip_suffix(']'))
                .and_then(|n| n.parse().ok())
                .map(Field::Str),
        };
        if let Some(new_field) = new_field {
            // A new entry starts with msgctxt, or with msgid unless msgctxt
            // was just read.
            let starts_entry = match new_field {
                Field::Context => true,
                Field::Id => !matches!(field, Field::Context),
                _ => false,
            };
            if starts_entry {
                if started {
                    entries.push(std::mem::take(&mut entry));
                }
                started = true;
                entry.fuzzy = std::mem::take(&mut fuzzy_next);
            }
            field = new_field;
            match field {
                Field::Context => entry.context = Some(value),
                Field::Id => entry.msgid = value,
                Field::Plural => entry.msgid_plural = Some(value),
                Field::Str(index) => {
                    if entry.msgstr.len() <= index {
                        entry.msgstr.resize(index + 1, String::new());
                    }
                    entry.msgstr[index] = value;
                }
                Field::None => {}
            }
        } else if keyword.is_empty() {
            match field {
                Field::Context => entry.context.get_or_insert_default().push_str(&value),
                Field::Id => entry.msgid.push_str(&value),
                Field::Plural => entry
                    .msgid_plural
                    .get_or_insert_default()
                    .push_str(&value),
                Field::Str(index) => {
                    if let Some(text) = entry.msgstr.get_mut(index) {
                        text.push_str(&value);
                    }
                }
                Field::None => {}
            }
        }
    }
    if started {
        entries.push(entry);
    }
    entries
}

/// Contents of a `"…"` token with C escapes resolved.
fn unquote(token: &str) -> String {
    let inner = token
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(token);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    #[test]
    fn russian_plural_forms() {
        let forms: Vec<usize> = [0, 1, 2, 4, 5, 11, 12, 14, 21, 22, 25, 101, 111]
            .iter()
            .map(|&n| plural_index(Lang::Ru, n))
            .collect();
        assert_eq!(forms, [2, 0, 1, 1, 2, 2, 2, 2, 0, 1, 2, 0, 2]);
        assert_eq!(plural_index(Lang::Es, 1), 0);
        assert_eq!(plural_index(Lang::Es, 0), 1);
    }

    #[test]
    fn placeholders() {
        let a: &dyn Display = &"a";
        let b: &dyn Display = &2;
        assert_eq!(format("{} and {}", &[a, b]), "a and 2");
        assert_eq!(format("{1} before {0}", &[a, b]), "2 before a");
        assert_eq!(format("{{x}} {}", &[a]), "{x} a");
        assert_eq!(format("missing {}", &[]), "missing {}");
        assert_eq!(format("{name}", &[a]), "{name}");
    }

    #[test]
    fn functions_without_catalog_return_english() {
        // Only the functions here: macro calls in this file would end up in
        // the catalog checks below.
        assert_eq!(tr("Cancel"), "Cancel");
        assert_eq!(trn("{} row", "{} rows", 1), "{} row");
        assert_eq!(trn("{} row", "{} rows", 3), "{} rows");
        assert_eq!(trc("column", "Name"), "Name");
    }

    #[test]
    fn language_codes() {
        assert_eq!(Lang::from_code("ru_RU.UTF-8"), Some(Lang::Ru));
        assert_eq!(Lang::from_code("es-MX"), Some(Lang::Es));
        assert_eq!(Lang::from_code("en_US@euro"), Some(Lang::En));
        assert_eq!(Lang::from_code("kk_KZ"), None);
        assert_eq!(Lang::from_code("system"), None);
    }

    #[test]
    fn po_reader() {
        let po = r#"
msgid ""
msgstr "Plural-Forms: nplurals=3;\n"

# comment
msgid "Line one "
"and two"
msgstr "Строка \"один\" "
"и два"

msgctxt "column"
msgid "Name"
msgstr "Имя"

#, fuzzy
msgid "Fuzzy"
msgstr "Нечёткая"

msgid "{} row"
msgid_plural "{} rows"
msgstr[0] "{} строка"
msgstr[1] "{} строки"
msgstr[2] "{} строк"
"#;
        let catalog = parse_po(po);
        assert_eq!(
            catalog.singular["Line one and two"],
            "Строка \"один\" и два"
        );
        assert_eq!(catalog.singular[&context_key("column", "Name")], "Имя");
        assert!(!catalog.singular.contains_key("Fuzzy"));
        assert_eq!(catalog.plural["{} row"][2], "{} строк");
    }

    // ----- Catalog checks against the source code ---------------------------

    /// A string the code asks to translate.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    struct Msg {
        context: Option<String>,
        msgid: String,
        plural: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Token {
        Ident(String),
        Str(String),
        Punct(char),
    }

    /// Just enough of a Rust lexer to find macro calls with string literals:
    /// skips comments and char literals, reads normal and raw strings.
    fn lex(source: &str) -> Vec<Token> {
        let chars: Vec<char> = source.chars().collect();
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() {
                i += 1;
            } else if c == '/' && chars.get(i + 1) == Some(&'/') {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            } else if c == '/' && chars.get(i + 1) == Some(&'*') {
                let mut depth = 0;
                while i < chars.len() {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            } else if c == '\'' {
                // Char literal or lifetime.
                if chars.get(i + 1) == Some(&'\\') {
                    i += 2;
                    while i < chars.len() && chars[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                } else if chars.get(i + 2) == Some(&'\'') {
                    i += 3;
                } else {
                    i += 1;
                }
            } else if c == '"' {
                let (text, end) = read_string(&chars, i + 1);
                tokens.push(Token::Str(text));
                i = end;
            } else if let Some((hashes, quote)) = raw_start(&chars, i) {
                let mut j = quote + 1;
                let mut text = String::new();
                while j < chars.len() {
                    if chars[j] == '"' && (0..hashes).all(|k| chars.get(j + 1 + k) == Some(&'#')) {
                        j += 1 + hashes;
                        break;
                    }
                    text.push(chars[j]);
                    j += 1;
                }
                tokens.push(Token::Str(text));
                i = j;
            } else if c == 'b' && chars.get(i + 1) == Some(&'"') {
                let (text, end) = read_string(&chars, i + 2);
                tokens.push(Token::Str(text));
                i = end;
            } else if c.is_alphanumeric() || c == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token::Ident(chars[start..i].iter().collect()));
            } else {
                tokens.push(Token::Punct(c));
                i += 1;
            }
        }
        tokens
    }

    /// For `r"`, `r#"`, `br"`…: number of `#` and index of the opening quote.
    /// Only at the start of a token (not inside an identifier like `bar"`).
    fn raw_start(chars: &[char], i: usize) -> Option<(usize, usize)> {
        if i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_') {
            return None;
        }
        let mut j = i;
        if chars.get(j) == Some(&'b') {
            j += 1;
        }
        if chars.get(j) != Some(&'r') {
            return None;
        }
        j += 1;
        let mut hashes = 0;
        while chars.get(j) == Some(&'#') {
            hashes += 1;
            j += 1;
        }
        (chars.get(j) == Some(&'"')).then_some((hashes, j))
    }

    /// Reads a normal string literal starting after the opening quote.
    fn read_string(chars: &[char], mut i: usize) -> (String, usize) {
        let mut text = String::new();
        while i < chars.len() {
            match chars[i] {
                '"' => return (text, i + 1),
                '\\' => {
                    i += 1;
                    match chars.get(i) {
                        Some('n') => text.push('\n'),
                        Some('t') => text.push('\t'),
                        Some('r') => text.push('\r'),
                        Some('0') => text.push('\0'),
                        Some('\n') => {
                            // Line continuation: skip the newline and the
                            // indentation that follows.
                            while chars.get(i + 1).is_some_and(|c| c.is_whitespace()) {
                                i += 1;
                            }
                        }
                        Some('u') => {
                            let close = (i..chars.len()).find(|&k| chars[k] == '}').unwrap_or(i);
                            let hex: String = chars[i + 2..close].iter().collect();
                            if let Some(ch) =
                                u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                            {
                                text.push(ch);
                            }
                            i = close;
                        }
                        Some(&other) => text.push(other),
                        None => {}
                    }
                    i += 1;
                }
                other => {
                    text.push(other);
                    i += 1;
                }
            }
        }
        (text, i)
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    fn sources() -> Vec<(PathBuf, String)> {
        let mut files = Vec::new();
        rust_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        files.sort();
        files
            .into_iter()
            .map(|path| {
                let text = std::fs::read_to_string(&path).unwrap();
                (path, text)
            })
            .collect()
    }

    /// The part of a file that ships: everything before its test module.
    fn shipped_part(text: &str) -> &str {
        text.find("#[cfg(test)]").map_or(text, |at| &text[..at])
    }

    /// Every string passed to tr!/trn!/trc!/n_! in the shipped code, with
    /// the file it was first seen in.
    fn messages() -> BTreeMap<Msg, String> {
        let mut found = BTreeMap::new();
        for (path, text) in sources() {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name == "i18n.rs" {
                continue;
            }
            let tokens = lex(shipped_part(&text));
            for (i, token) in tokens.iter().enumerate() {
                let Token::Ident(ident) = token else { continue };
                if !matches!(ident.as_str(), "tr" | "trn" | "trc" | "n_")
                    || tokens.get(i + 1) != Some(&Token::Punct('!'))
                    || tokens.get(i + 2) != Some(&Token::Punct('('))
                {
                    continue;
                }
                let str_at = |k: usize| match tokens.get(i + 3 + k) {
                    Some(Token::Str(text)) => Some(text.clone()),
                    _ => None,
                };
                let comma_at = |k: usize| tokens.get(i + 3 + k) == Some(&Token::Punct(','));
                let msg = match ident.as_str() {
                    "trc" => match (str_at(0), comma_at(1), str_at(2)) {
                        (Some(context), true, Some(msgid)) => Some(Msg {
                            context: Some(context),
                            msgid,
                            plural: None,
                        }),
                        _ => None,
                    },
                    "trn" => match (str_at(0), comma_at(1), str_at(2)) {
                        (Some(msgid), true, Some(plural)) => Some(Msg {
                            context: None,
                            msgid,
                            plural: Some(plural),
                        }),
                        _ => None,
                    },
                    _ => str_at(0).map(|msgid| Msg {
                        context: None,
                        msgid,
                        plural: None,
                    }),
                };
                let msg = msg.unwrap_or_else(|| panic!("{name}: {ident}! must take string literals"));
                found.entry(msg).or_insert_with(|| name.clone());
            }
        }
        found
    }

    /// Placeholders of a template as a sorted list: `{}` counts by position,
    /// `{N}` is N.
    fn placeholders_of(text: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut next = 0;
        let chars: Vec<char> = text.replace("{{", "").replace("}}", "").chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '{' {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                if chars.get(j) == Some(&'}') {
                    let index: String = chars[i + 1..j].iter().collect();
                    if index.is_empty() {
                        found.push(next.to_string());
                        next += 1;
                    } else {
                        found.push(index);
                    }
                    i = j;
                }
            }
            i += 1;
        }
        found.sort();
        found
    }

    fn check_catalog(file: &str, data: &str, forms: usize) {
        let messages = messages();
        let entries: Vec<PoEntry> = po_entries(data)
            .into_iter()
            .filter(|entry| !entry.msgid.is_empty())
            .collect();
        let mut problems = Vec::new();
        let mut seen = BTreeSet::new();
        for entry in &entries {
            let msg = Msg {
                context: entry.context.clone(),
                msgid: entry.msgid.clone(),
                plural: entry.msgid_plural.clone(),
            };
            if !seen.insert(msg.clone()) {
                problems.push(format!("duplicate entry {:?}", entry.msgid));
            }
            if !messages.contains_key(&msg) {
                problems.push(format!("unused entry {:?}", entry.msgid));
                continue;
            }
            if entry.fuzzy {
                problems.push(format!("fuzzy entry {:?}", entry.msgid));
            }
            let expected = if msg.plural.is_some() { forms } else { 1 };
            if entry.msgstr.len() != expected || entry.msgstr.iter().any(String::is_empty) {
                problems.push(format!("untranslated {:?}", entry.msgid));
                continue;
            }
            for (index, text) in entry.msgstr.iter().enumerate() {
                let source = match &msg.plural {
                    Some(plural) if index > 0 => plural,
                    _ => &msg.msgid,
                };
                let want = placeholders_of(source);
                let got = placeholders_of(text);
                // A plural form may drop the number ("одна строка"), but must
                // not invent placeholders.
                let ok = if msg.plural.is_some() {
                    got.iter().all(|p| want.contains(p))
                } else {
                    got == want
                };
                if !ok {
                    problems.push(format!(
                        "placeholders differ in {:?}: {:?}",
                        entry.msgid, text
                    ));
                }
            }
        }
        for (msg, source) in &messages {
            if !seen.contains(msg) {
                problems.push(format!("missing {:?} (from {source})", msg.msgid));
            }
        }
        assert!(problems.is_empty(), "{file}:\n{}", problems.join("\n"));
    }

    #[test]
    fn russian_catalog_is_complete() {
        check_catalog("po/ru.po", RU_PO, 3);
    }

    #[test]
    fn spanish_catalog_is_complete() {
        check_catalog("po/es.po", ES_PO, 2);
    }

    /// Text for people must go through tr!; Cyrillic literals left in the
    /// shipped code mean a string was missed.
    #[test]
    fn no_untranslated_literals() {
        let mut problems = Vec::new();
        for (path, text) in sources() {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name == "i18n.rs" || name == "live_tests.rs" {
                continue;
            }
            for token in lex(shipped_part(&text)) {
                if let Token::Str(text) = token
                    && text
                        .chars()
                        .any(|c| matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё'))
                {
                    problems.push(format!("{name}: {text:?}"));
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
