//! Разбор `~/.ssh/config` в объёме, нужном для списка хостов.
//!
//! Это не полноценная реализация ssh_config(5): само подключение выполняет
//! системный `ssh`, который читает конфиг сам. Здесь извлекаются только
//! псевдонимы из строк `Host` (без масок и отрицаний) с учётом `Include`,
//! плюс `HostName`/`User`/`Port` для подписи в списке.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// OpenSSH ограничивает вложенность `Include` шестнадцатью уровнями.
const MAX_INCLUDE_DEPTH: usize = 16;

/// Запись списка хостов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    /// Псевдоним из строки `Host`, именно он передаётся в `ssh`.
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<String>,
}

impl HostEntry {
    /// Подпись вида `user@hostname:port`; пусто, если уточнять нечего.
    pub fn detail(&self) -> String {
        let mut out = String::new();
        if let Some(user) = &self.user {
            out.push_str(user);
            out.push('@');
        }
        match &self.hostname {
            Some(hostname) => out.push_str(hostname),
            None if !out.is_empty() || self.port.is_some() => out.push_str(&self.alias),
            None => {}
        }
        if let Some(port) = &self.port {
            out.push(':');
            out.push_str(port);
        }
        if out == self.alias { String::new() } else { out }
    }
}

/// Результат разбора: хосты в порядке появления и некритичные предупреждения.
#[derive(Debug, Default)]
pub struct ParsedConfig {
    pub hosts: Vec<HostEntry>,
    pub warnings: Vec<String>,
}

/// Читает конфиг `config_path`. `home` нужен для `~` и относительных `Include`
/// (они, как в OpenSSH, считаются от `~/.ssh`).
///
/// Отсутствие основного файла ошибкой не считается: список просто пуст.
pub fn load(config_path: &Path, home: &Path) -> ParsedConfig {
    let mut parser = Parser {
        home: home.to_path_buf(),
        hosts: Vec::new(),
        index: HashMap::new(),
        warnings: Vec::new(),
    };
    if config_path.exists() {
        parser.parse_file(config_path, 0);
    }
    ParsedConfig {
        hosts: parser.hosts,
        warnings: parser.warnings,
    }
}

struct Parser {
    home: PathBuf,
    hosts: Vec<HostEntry>,
    /// Псевдоним -> позиция в `hosts`, чтобы объединять повторные блоки.
    index: HashMap<String, usize>,
    warnings: Vec<String>,
}

impl Parser {
    fn parse_file(&mut self, path: &Path, depth: usize) {
        if depth > MAX_INCLUDE_DEPTH {
            self.warnings
                .push(tr!("{}: Include is nested too deeply", path.display()));
            return;
        }
        let text = match fs::read(path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(err) => {
                self.warnings
                    .push(tr!("{}: could not read: {}", path.display(), err));
                return;
            }
        };

        // Хосты текущего блока `Host`; пусто вне блока или внутри `Match`.
        let mut current: Vec<usize> = Vec::new();

        for line in text.lines() {
            let Some((keyword, args)) = split_line(line) else {
                continue;
            };
            match keyword.as_str() {
                "host" => {
                    current.clear();
                    for pattern in &args {
                        if is_concrete_host(pattern) {
                            current.push(self.intern(pattern));
                        }
                    }
                }
                "match" => current.clear(),
                "include" => {
                    for arg in &args {
                        for file in self.resolve_include(arg) {
                            self.parse_file(&file, depth + 1);
                        }
                    }
                }
                "hostname" => self.set_field(&current, &args, |h| &mut h.hostname),
                "user" => self.set_field(&current, &args, |h| &mut h.user),
                "port" => self.set_field(&current, &args, |h| &mut h.port),
                _ => {}
            }
        }
    }

    fn intern(&mut self, alias: &str) -> usize {
        if let Some(&idx) = self.index.get(alias) {
            return idx;
        }
        let idx = self.hosts.len();
        self.hosts.push(HostEntry {
            alias: alias.to_string(),
            hostname: None,
            user: None,
            port: None,
        });
        self.index.insert(alias.to_string(), idx);
        idx
    }

    /// Как и в ssh, действует первое встреченное значение параметра.
    fn set_field(
        &mut self,
        current: &[usize],
        args: &[String],
        field: impl Fn(&mut HostEntry) -> &mut Option<String>,
    ) {
        let Some(value) = args.first() else { return };
        for &idx in current {
            let slot = field(&mut self.hosts[idx]);
            if slot.is_none() {
                *slot = Some(value.clone());
            }
        }
    }

    fn resolve_include(&self, arg: &str) -> Vec<PathBuf> {
        let expanded = if arg == "~" {
            self.home.clone()
        } else if let Some(rest) = arg.strip_prefix("~/") {
            self.home.join(rest)
        } else {
            PathBuf::from(arg)
        };
        let absolute = if expanded.is_absolute() {
            expanded
        } else {
            self.home.join(".ssh").join(expanded)
        };
        expand_glob(&absolute)
    }
}

/// Хост пригоден для списка, если это не маска и не отрицание.
fn is_concrete_host(pattern: &str) -> bool {
    !pattern.is_empty() && !pattern.starts_with('!') && !pattern.contains(['*', '?'])
}

/// Делит строку конфига на ключевое слово (в нижнем регистре) и аргументы.
/// Понимает `Key Value`, `Key=Value`, двойные кавычки и комментарии.
fn split_line(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let key_end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let keyword = line[..key_end].to_ascii_lowercase();
    let rest = line[key_end..].trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    Some((keyword, split_args(rest)))
}

fn split_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut chars = input.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        if first == '#' {
            break;
        }
        let mut arg = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if c == '"' {
                quoted = !quoted;
            } else if c.is_whitespace() && !quoted {
                break;
            } else {
                arg.push(c);
            }
            chars.next();
        }
        args.push(arg);
    }
    args
}

/// Раскрывает `*` и `?` в компонентах пути. Путь без масок возвращается,
/// только если файл существует (ssh молча пропускает отсутствующие Include).
fn expand_glob(pattern: &Path) -> Vec<PathBuf> {
    let mut results = vec![PathBuf::new()];
    for component in pattern.components() {
        let part = component.as_os_str().to_string_lossy();
        let mut next = Vec::new();
        if part.contains(['*', '?']) {
            for base in &results {
                let Ok(entries) = fs::read_dir(base) else {
                    continue;
                };
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|name| wildcard_match(&part, name))
                    .collect();
                names.sort();
                next.extend(names.into_iter().map(|name| base.join(name)));
            }
        } else {
            next.extend(results.iter().map(|base| base.join(component)));
        }
        results = next;
    }
    results.retain(|p| p.is_file());
    results
}

/// Сопоставление с маской в стиле glob(3): `*`, `?`; скрытые файлы
/// подходят, только если маска сама начинается с точки.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    if name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            backtrack = Some((pi, ni));
            pi += 1;
        } else if let Some((star_p, star_n)) = backtrack {
            pi = star_p + 1;
            ni = star_n + 1;
            backtrack = Some((star_p, star_n + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Создаёт «домашний каталог» с `.ssh/config` и возвращает результат разбора.
    fn parse(config: &str, extra: &[(&str, &str)]) -> ParsedConfig {
        let home = TempDir::new().expect("tempdir");
        let ssh = home.path().join(".ssh");
        fs::create_dir_all(&ssh).expect("mkdir .ssh");
        fs::write(ssh.join("config"), config).expect("write config");
        for (rel, content) in extra {
            let path = home.path().join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("mkdir include dir");
            }
            fs::write(path, content).expect("write include");
        }
        load(&ssh.join("config"), home.path())
    }

    fn aliases(parsed: &ParsedConfig) -> Vec<&str> {
        parsed.hosts.iter().map(|h| h.alias.as_str()).collect()
    }

    #[test]
    fn simple_hosts_in_order() {
        let parsed = parse("Host alpha\n  HostName a.example\nHost beta\n", &[]);
        assert_eq!(aliases(&parsed), ["alpha", "beta"]);
        assert!(parsed.warnings.is_empty());
    }

    #[test]
    fn wildcards_and_negations_are_skipped() {
        let parsed = parse(
            "Host *\n  User all\nHost web-* !web-old db?\nHost real *.internal\n",
            &[],
        );
        assert_eq!(aliases(&parsed), ["real"]);
    }

    #[test]
    fn multiple_aliases_on_one_line_share_settings() {
        let parsed = parse("Host one two\n  User deploy\n  Port 2200\n", &[]);
        assert_eq!(aliases(&parsed), ["one", "two"]);
        for host in &parsed.hosts {
            assert_eq!(host.user.as_deref(), Some("deploy"));
            assert_eq!(host.port.as_deref(), Some("2200"));
        }
    }

    #[test]
    fn keywords_are_case_insensitive_and_accept_equals() {
        let parsed = parse("HOST=gamma\n\thostNAME = 10.0.0.5\n  user=root\n", &[]);
        assert_eq!(aliases(&parsed), ["gamma"]);
        assert_eq!(parsed.hosts[0].hostname.as_deref(), Some("10.0.0.5"));
        assert_eq!(parsed.hosts[0].user.as_deref(), Some("root"));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let parsed = parse(
            "# Host commented\n\nHost delta # trailing comment\n  # User nobody\n  User real\n",
            &[],
        );
        assert_eq!(aliases(&parsed), ["delta"]);
        assert_eq!(parsed.hosts[0].user.as_deref(), Some("real"));
    }

    #[test]
    fn quoted_arguments() {
        let parsed = parse("Host \"quoted\" plain\n  HostName \"h.example\"\n", &[]);
        assert_eq!(aliases(&parsed), ["quoted", "plain"]);
        assert_eq!(parsed.hosts[0].hostname.as_deref(), Some("h.example"));
    }

    #[test]
    fn duplicate_hosts_are_merged_first_value_wins() {
        let parsed = parse(
            "Host dup\n  User first\nHost other\nHost dup\n  User second\n  Port 2222\n",
            &[],
        );
        assert_eq!(aliases(&parsed), ["dup", "other"]);
        assert_eq!(parsed.hosts[0].user.as_deref(), Some("first"));
        assert_eq!(parsed.hosts[0].port.as_deref(), Some("2222"));
    }

    #[test]
    fn match_block_ends_host_block() {
        let parsed = parse(
            "Host epsilon\nMatch user root\n  HostName wrong.example\n",
            &[],
        );
        assert_eq!(aliases(&parsed), ["epsilon"]);
        assert_eq!(parsed.hosts[0].hostname, None);
    }

    #[test]
    fn global_options_before_first_host_do_not_attach() {
        let parsed = parse("User globaluser\nHost zeta\n", &[]);
        assert_eq!(parsed.hosts[0].user, None);
    }

    #[test]
    fn include_relative_is_resolved_against_dot_ssh() {
        let parsed = parse(
            "Include extra.conf\nHost main\n",
            &[(".ssh/extra.conf", "Host included\n  HostName inc.example\n")],
        );
        assert_eq!(aliases(&parsed), ["included", "main"]);
        assert_eq!(parsed.hosts[0].hostname.as_deref(), Some("inc.example"));
    }

    #[test]
    fn include_glob_expands_sorted_and_skips_hidden() {
        let parsed = parse(
            "Include conf.d/*.conf\n",
            &[
                (".ssh/conf.d/20-b.conf", "Host b\n"),
                (".ssh/conf.d/10-a.conf", "Host a\n"),
                (".ssh/conf.d/.hidden.conf", "Host hidden\n"),
                (".ssh/conf.d/readme.txt", "Host txt\n"),
            ],
        );
        assert_eq!(aliases(&parsed), ["a", "b"]);
    }

    #[test]
    fn include_tilde_and_multiple_arguments() {
        let parsed = parse(
            "Include ~/cfg/one.conf two.conf\n",
            &[("cfg/one.conf", "Host one\n"), (".ssh/two.conf", "Host two\n")],
        );
        assert_eq!(aliases(&parsed), ["one", "two"]);
    }

    #[test]
    fn include_absolute_path() {
        let outside = TempDir::new().expect("tempdir");
        let file = outside.path().join("abs.conf");
        fs::write(&file, "Host absolute\n").expect("write");
        let parsed = parse(&format!("Include {}\n", file.display()), &[]);
        assert_eq!(aliases(&parsed), ["absolute"]);
    }

    #[test]
    fn nested_include() {
        let parsed = parse(
            "Include level1.conf\n",
            &[
                (".ssh/level1.conf", "Host l1\nInclude level2.conf\n"),
                (".ssh/level2.conf", "Host l2\n"),
            ],
        );
        assert_eq!(aliases(&parsed), ["l1", "l2"]);
    }

    #[test]
    fn missing_include_is_silently_skipped() {
        let parsed = parse("Include nope.conf\nHost ok\n", &[]);
        assert_eq!(aliases(&parsed), ["ok"]);
        assert!(parsed.warnings.is_empty());
    }

    #[test]
    fn include_cycle_terminates_with_warning() {
        let parsed = parse("Host loop\nInclude config\n", &[]);
        assert_eq!(aliases(&parsed), ["loop"]);
        assert_eq!(parsed.warnings.len(), 1);
    }

    #[test]
    fn missing_config_gives_empty_list() {
        let home = TempDir::new().expect("tempdir");
        let parsed = load(&home.path().join(".ssh/config"), home.path());
        assert!(parsed.hosts.is_empty());
        assert!(parsed.warnings.is_empty());
    }

    #[test]
    fn detail_formats() {
        let mut host = HostEntry {
            alias: "srv".into(),
            hostname: Some("10.1.1.1".into()),
            user: Some("root".into()),
            port: Some("2222".into()),
        };
        assert_eq!(host.detail(), "root@10.1.1.1:2222");
        host.user = None;
        host.port = None;
        assert_eq!(host.detail(), "10.1.1.1");
        host.hostname = None;
        assert_eq!(host.detail(), "");
        host.user = Some("bob".into());
        assert_eq!(host.detail(), "bob@srv");
        host.user = None;
        host.hostname = Some("srv".into());
        assert_eq!(host.detail(), "");
    }

    #[test]
    fn wildcard_matcher() {
        assert!(wildcard_match("*.conf", "a.conf"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(wildcard_match("file?", "file1"));
        assert!(!wildcard_match("file?", "file"));
        assert!(!wildcard_match("*.conf", "a.txt"));
        assert!(!wildcard_match("*", ".hidden"));
        assert!(wildcard_match(".*", ".hidden"));
        assert!(wildcard_match("*", ""));
    }
}
