//! Удалённые команды через уже авторизованное мастер-соединение вкладки.
//!
//! Команды идут отдельным каналом того же соединения: пароль не нужен, а
//! терминал пользователя они не затрагивают. Функции блокирующие — вызывать
//! их из потока GTK нельзя.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::session::Session;

/// Оборачивает сценарий для удалённой оболочки входа (она может быть любой,
/// например fish): сценарий выполняет `sh`. В тексте не должно быть
/// одинарных кавычек.
pub fn sh(script: &str) -> String {
    debug_assert!(!script.contains('\''), "script contains a single quote");
    format!("sh -c '{script}'")
}

pub struct Output {
    pub stdout: String,
    pub stderr: String,
}

/// Жив ли мастер сессии.
pub fn master_alive(session: &Session) -> bool {
    let check = session.check_argv();
    Command::new(&check[0])
        .args(&check[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Выполняет команду, подав `input` на её стандартный ввод, и ждёт
/// завершения.
pub fn run(session: &Session, command: &str, input: &str) -> Result<Output, String> {
    if !master_alive(session) {
        return Err(tr!(
            "No active connection. Log in to the server in this tab’s terminal first."
        )
        .to_string());
    }
    let argv = session.command_argv(command);
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| tr!("Could not start ssh: {}", err))?;
    if let Some(mut stdin) = child.stdin.take() {
        // Ошибка записи проявится как отсутствие ожидаемого ответа.
        let _ = stdin.write_all(input.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("ssh: {err}"))?;
    Ok(Output {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Долгоживущая удалённая команда, вывод которой читается построчно.
/// Команда останавливается, когда значение уничтожается.
pub struct Stream {
    child: Arc<Mutex<Child>>,
}

impl Stream {
    pub fn is_running(&self) -> bool {
        self.child
            .lock()
            .is_ok_and(|mut child| matches!(child.try_wait(), Ok(None)))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Запускает команду и передаёт каждую строку её вывода в `on_line` (из
/// отдельного потока). Поток завершается вместе с командой или когда
/// `on_line` вернёт `false`.
pub fn stream(
    session: &Session,
    command: &str,
    mut on_line: impl FnMut(String) -> bool + Send + 'static,
) -> Result<Stream, String> {
    let argv = session.command_argv(command);
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| tr!("Could not start ssh: {}", err))?;
    let stdout = child.stdout.take();
    std::thread::spawn(move || {
        let Some(stdout) = stdout else { return };
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if !on_line(line) {
                break;
            }
        }
    });
    Ok(Stream {
        child: Arc::new(Mutex::new(child)),
    })
}

/// Строка в одинарных кавычках для оболочки (bash, zsh, fish, sh).
/// В fish внутри одинарных кавычек особые `\\` и `\'`, поэтому строку с
/// обратной косой чертой fish прочтёт иначе (см. [`cd_command`]).
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Команда перехода терминала в удалённый каталог `dir`, как её набрал бы
/// пользователь. Пробел в начале не даёт команде попасть в историю
/// оболочки (там, где это настроено).
///
/// Какая оболочка сейчас работает в терминале, неизвестно (пользователь
/// мог запустить другую, `sudo -i` или `ssh` дальше), поэтому команда
/// должна одинаково читаться в sh (dash, ash), bash, zsh, ksh и fish:
///
/// - обычный путь (UTF-8 без управляющих символов и `\`) — просто в
///   одинарных кавычках: `cd '/var/log'`;
/// - иначе путь собирает `printf`: `cd "$(printf '/srv/\377\376')"`.
///   Восьмеричные `\NNN` понимают printf в POSIX, bash, zsh и fish, а `$(…)`
///   в двойных кавычках — все эти оболочки (fish — с версии 3.4). Через
///   `\NNN` идут байты не из UTF-8, управляющие символы (перевод строки или
///   Tab, набранные в терминале, сработали бы как Enter или дополнение),
///   а также `\`, `'`, `%` (особые для printf и кавычек) и `!` (подстановка
///   из истории bash и zsh действует и внутри `"$(…)"`).
pub fn cd_command(dir: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = dir.as_os_str().as_bytes();
    let dotted;
    if bytes.starts_with(b"-") {
        // Иначе cd примет путь за ключ.
        dotted = [b"./", bytes].concat();
        bytes = &dotted;
    }
    if let Ok(text) = std::str::from_utf8(bytes)
        && !text.chars().any(|c| c.is_control() || c == '\\')
    {
        return format!(" cd {}", shell_quote(text));
    }
    let mut format = String::new();
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c.is_control() || matches!(c, '\\' | '\'' | '%' | '!') {
                let mut buf = [0; 4];
                for byte in c.encode_utf8(&mut buf).bytes() {
                    format.push_str(&format!("\\{byte:03o}"));
                }
            } else {
                format.push(c);
            }
        }
        for byte in chunk.invalid() {
            format.push_str(&format!("\\{byte:03o}"));
        }
    }
    if bytes.ends_with(b"\n") {
        // `$(…)` отбрасывает переводы строки в конце вывода, а косая черта
        // после имени каталога ничего не меняет.
        format.push('/');
    }
    format!(" cd \"$(printf '{format}')\"")
}

// ------------------------------------------------- слежение за каталогом

const CWD_ADDED: &str = "SSHDECK_CWD_ADDED";
const CWD_UPDATED: &str = "SSHDECK_CWD_UPDATED";
const CWD_PRESENT: &str = "SSHDECK_CWD_PRESENT";
const CWD_BUILTIN: &str = "SSHDECK_CWD_BUILTIN";
const CWD_UNSUPPORTED: &str = "SSHDECK_CWD_UNSUPPORTED";

/// Дописывает в файл настроек оболочки то, что пришло на стандартный ввод.
/// Метка версии `sshdeck-cwd v2` — та же, что в [`CWD_SNIPPET`]; при
/// изменении фрагмента номер версии повышается в обоих местах, и старый
/// фрагмент на сервере заменяется новым.
///
/// Если там уже есть текущая версия фрагмента — ничего не меняет; если
/// старая (строки с `sshdeck-cwd` или `__sshdeck_cwd`) — убирает её и
/// дописывает новую. Файл переписывается на месте (`cat >`), чтобы
/// сохранились его права и символическая ссылка. Без одинарных кавычек.
const CWD_SCRIPT: &str = r#"case "$(basename "${SHELL:-sh}")" in bash) f=.bashrc;; zsh) f=.zshrc;; fish) echo SSHDECK_CWD_BUILTIN; exit 0;; *) echo SSHDECK_CWD_UNSUPPORTED; exit 0;; esac; f="$HOME/$f"; if grep -qF "sshdeck-cwd v2" "$f" 2>/dev/null; then echo SSHDECK_CWD_PRESENT; elif grep -qF sshdeck-cwd "$f" 2>/dev/null; then t="$f.sshdeck-tmp"; sed -e "/sshdeck-cwd/d" -e "/__sshdeck_cwd/d" "$f" > "$t" && cat "$t" > "$f" && rm -f "$t" && cat >> "$f" && echo SSHDECK_CWD_UPDATED; else cat >> "$f" && echo SSHDECK_CWD_ADDED; fi"#;

/// Фрагмент для `.bashrc`/`.zshrc`: перед каждым приглашением сообщает
/// терминалу текущий каталог (последовательность OSC 7).
///
/// Путь в URI кодируется процентами побайтно: `LC_ALL=C` заставляет bash и
/// zsh перебирать байты, а не символы, поэтому UTF-8 и любые другие байты
/// проходят без искажений, а пробелы и `%` не ломают URI. Код байта
/// берётся по модулю 256: в C-локали musl (Alpine) байты 0x80–0xFF дают
/// коды U+DF80–U+DFFF, а где-то код может оказаться отрицательным. Внешние
/// программы не нужны. Каждая строка содержит `sshdeck-cwd` или `__sshdeck_cwd` —
/// по ним сценарий установки находит и заменяет фрагмент.
const CWD_SNIPPET: &str = r#"
# sshdeck-cwd v2: report the current directory to the terminal (OSC 7)
__sshdeck_cwd() { local LC_ALL=C p="$PWD" u= c i; for ((i = 0; i < ${#p}; i++)); do c="${p:$i:1}"; case "$c" in [a-zA-Z0-9/._~-]) u+="$c";; *) printf -v c %d "'$c"; printf -v c %%%02X $((c & 255)); u+="$c";; esac; done; printf '\033]7;file://%s%s\033\\' "${HOSTNAME:-$HOST}" "$u"; }
if [ -n "${ZSH_VERSION-}" ]; then precmd_functions+=(__sshdeck_cwd); else PROMPT_COMMAND="__sshdeck_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"; fi
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CwdTracking {
    /// Фрагмент дописан; заработает в новых сеансах оболочки.
    Added,
    /// Старая версия фрагмента заменена новой.
    Updated,
    /// Фрагмент текущей версии уже был на месте.
    Present,
    /// Оболочка (fish) сообщает каталог сама.
    Builtin,
    /// Оболочка не bash, не zsh и не fish.
    Unsupported,
}

/// Включает на сервере сообщение текущего каталога терминалу.
pub fn install_cwd_tracking(session: &Session) -> Result<CwdTracking, String> {
    let output = run(session, &sh(CWD_SCRIPT), CWD_SNIPPET)?;
    parse_cwd_reply(&output.stdout).ok_or_else(|| {
        let detail = output.stderr.trim();
        if detail.is_empty() {
            tr!("The server did not confirm the shell settings change").to_string()
        } else {
            tr!("Could not change the shell settings: {}", detail)
        }
    })
}

fn parse_cwd_reply(stdout: &str) -> Option<CwdTracking> {
    stdout.lines().find_map(|line| match line.trim() {
        CWD_ADDED => Some(CwdTracking::Added),
        CWD_UPDATED => Some(CwdTracking::Updated),
        CWD_PRESENT => Some(CwdTracking::Present),
        CWD_BUILTIN => Some(CwdTracking::Builtin),
        CWD_UNSUPPORTED => Some(CwdTracking::Unsupported),
        _ => None,
    })
}

/// Каталог из OSC 7: хост и путь из URI `file://host/path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedDir {
    /// Хост, который назвала оболочка (может быть пустым).
    pub host: String,
    /// Путь после раскодирования процентов, как набор байтов.
    pub path: Vec<u8>,
}

/// Разбирает URI текущего каталога, присланный оболочкой (OSC 7):
/// `file://host/path`, где путь закодирован процентами. Раскодированный
/// путь — сырые байты: имя файла не обязано быть в UTF-8. Нераспознанное
/// `%` (без двух шестнадцатеричных цифр) остаётся как есть — так пишут
/// старые версии фрагмента. Запрос (`?…`) и фрагмент (`#…`) в URI каталога
/// не бывают, поэтому `#` и `?` считаются частью пути.
pub fn parse_cwd_uri(uri: &str) -> Option<ReportedDir> {
    let rest = uri.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let (host, path) = rest.split_at(slash);
    Some(ReportedDir {
        host: String::from_utf8_lossy(&percent_decode(host)).into_owned(),
        path: percent_decode(path),
    })
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(&hi), Some(&lo)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(hi), Some(lo)) = (hex(hi), hex(lo))
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Тот ли это сервер, что у вкладки. `server` — имя хоста вкладки (`uname -n`
/// или первое сообщение OSC 7), `reported` — хост из очередного OSC 7.
/// Пустое имя и `localhost` ничего не говорят о сервере и принимаются;
/// короткое имя совпадает с полным (`web1` и `web1.example.com`). Регистр
/// не важен.
pub fn same_host(server: &str, reported: &str) -> bool {
    let server = server.trim().to_ascii_lowercase();
    let reported = reported.trim().to_ascii_lowercase();
    if server.is_empty() || reported.is_empty() || reported == "localhost" {
        return true;
    }
    let extends = |long: &str, short: &str| {
        long.strip_prefix(short)
            .is_some_and(|rest| rest.starts_with('.'))
    };
    server == reported || extends(&server, &reported) || extends(&reported, &server)
}

/// Имя сервера, как его видит он сам (`uname -n` — то же, что `$HOSTNAME`
/// в bash и `$HOST` в zsh). Через мастер-соединение, блокирующая.
pub fn server_hostname(session: &Session) -> Result<String, String> {
    let output = run(session, "uname -n", "")?;
    let name = output.stdout.lines().last().unwrap_or_default().trim();
    if name.is_empty() {
        Err(output.stderr.trim().to_string())
    } else {
        Ok(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::*;

    #[test]
    fn script_is_wrapped_for_sh() {
        assert_eq!(sh("echo \"$HOME\""), "sh -c 'echo \"$HOME\"'");
        assert!(!CWD_SCRIPT.contains('\''));
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/var/log"), "'/var/log'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    fn cd(bytes: &[u8]) -> String {
        use std::os::unix::ffi::OsStrExt;
        cd_command(Path::new(std::ffi::OsStr::from_bytes(bytes)))
    }

    #[test]
    fn cd_command_quoting() {
        // Обычные пути — как раньше, в одинарных кавычках.
        assert_eq!(cd(b"/var/log"), " cd '/var/log'");
        assert_eq!(cd(b"/srv/my files"), " cd '/srv/my files'");
        assert_eq!(cd(b"/srv/it's"), r" cd '/srv/it'\''s'");
        assert_eq!(cd(b"/srv/$HOME `id` \"q\""), " cd '/srv/$HOME `id` \"q\"'");
        assert_eq!(cd(b"/srv/a!b%c"), " cd '/srv/a!b%c'");
        assert_eq!(cd("/srv/Документы".as_bytes()), " cd '/srv/Документы'");
        assert_eq!(cd(b"-rf"), " cd './-rf'");
        // Остальное собирает printf.
        assert_eq!(cd(b"/srv/\xff\xfe"), r#" cd "$(printf '/srv/\377\376')""#);
        assert_eq!(cd(b"/srv/a\\b"), r#" cd "$(printf '/srv/a\134b')""#);
        assert_eq!(cd(b"/srv/a\nb"), r#" cd "$(printf '/srv/a\012b')""#);
        assert_eq!(cd(b"/srv/a\tb"), r#" cd "$(printf '/srv/a\011b')""#);
        assert_eq!(cd(b"/srv/a\n"), r#" cd "$(printf '/srv/a\012/')""#);
        // Кириллица остаётся читаемой и рядом с байтами не из UTF-8.
        let mixed = ["/srv/Документы/".as_bytes(), b"\xe9t\xe9 it's 100% !$x"].concat();
        assert_eq!(
            cd(&mixed),
            r#" cd "$(printf '/srv/Документы/\351t\351 it\047s 100\045 \041$x')""#
        );
        assert_eq!(cd(b"-\xff"), r#" cd "$(printf './-\377')""#);
    }

    /// Команды из [`cd_command`] выполняются настоящими оболочками, какие
    /// найдутся; `pwd` после них должен показать нужный каталог байт в байт.
    #[test]
    fn cd_command_in_real_shells() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        let names: Vec<Vec<u8>> = vec![
            b"plain".to_vec(),
            b"my files".to_vec(),
            b"it's".to_vec(),
            b"$HOME `id` \"q\" (x) & y; z | w > v * ? [a] {b} #c ~d".to_vec(),
            b"a\\b".to_vec(),
            b"a\\".to_vec(),
            b"a\\\\'b".to_vec(),
            b"a\nb".to_vec(),
            b"trail\n".to_vec(),
            b"tab\tx".to_vec(),
            b"esc\x1b[31m".to_vec(),
            b"-dash".to_vec(),
            b"100% !x !! ^y".to_vec(),
            "Документы".as_bytes().to_vec(),
            b"\xff\xfe".to_vec(),
            ["Документы ".as_bytes(), b"\xe9t\xe9 it's 100% !x\\"].concat(),
        ];
        for name in &names {
            std::fs::create_dir(root.join(OsStr::from_bytes(name))).expect("mkdir");
        }
        let shells: [(&str, &[&str]); 7] = [
            ("sh", &["-c"]),
            ("bash", &["--norc", "-c"]),
            ("dash", &["-c"]),
            ("zsh", &["-f", "-c"]),
            ("ksh", &["-c"]),
            ("mksh", &["-c"]),
            ("fish", &["--no-config", "-c"]),
        ];
        let mut tested = Vec::new();
        for (shell, args) in shells {
            for locale in [None, Some("C")] {
                for name in &names {
                    let path = root.join(OsStr::from_bytes(name));
                    // Путь с `-` в начале бывает только относительным.
                    let target = if name.starts_with(b"-") {
                        Path::new(OsStr::from_bytes(name)).to_path_buf()
                    } else {
                        path.clone()
                    };
                    let script = format!("{}\npwd", cd_command(&target));
                    let mut command = Command::new(shell);
                    command.args(args).arg(&script).current_dir(&root);
                    if let Some(locale) = locale {
                        command.env("LC_ALL", locale);
                    }
                    let output = match command.output() {
                        Ok(output) => output,
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
                        Err(err) => panic!("{shell}: {err}"),
                    };
                    let mut expected = path.as_os_str().as_bytes().to_vec();
                    expected.push(b'\n');
                    assert!(
                        output.status.success() && output.stdout == expected,
                        "{shell} (LC_ALL={locale:?}): {script:?}\nstdout: {:?}\nstderr: {}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                    if !tested.contains(&shell) {
                        tested.push(shell);
                    }
                }
            }
        }
        eprintln!("cd_command проверена в: {tested:?}");
        assert!(tested.contains(&"sh"), "не нашлось даже sh");
    }

    /// Выполняет удалённый сценарий локально с подменёнными HOME и SHELL.
    fn run_cwd_script(home: &Path, shell: &str) -> Option<CwdTracking> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(CWD_SCRIPT)
            .env("HOME", home)
            .env("SHELL", shell)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("sh");
        // Сценарий читает ввод не во всех ветках, поэтому запись может
        // упереться в закрытый канал — это не ошибка.
        let _ = child
            .stdin
            .take()
            .expect("stdin")
            .write_all(CWD_SNIPPET.as_bytes());
        let output = child.wait_with_output().expect("wait");
        parse_cwd_reply(&String::from_utf8_lossy(&output.stdout))
    }

    /// Первая версия фрагмента (до кодирования пути) — такой она лежит в
    /// `.bashrc` у тех, кто включил слежение раньше.
    const CWD_SNIPPET_V1: &str = r#"
# sshdeck-cwd: report the current directory to the terminal (OSC 7)
__sshdeck_cwd() { printf '\033]7;file://%s%s\033\\' "${HOSTNAME:-$HOST}" "$PWD"; }
if [ -n "$ZSH_VERSION" ]; then precmd_functions+=(__sshdeck_cwd); else PROMPT_COMMAND="__sshdeck_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"; fi
"#;

    /// Метка текущей версии фрагмента.
    const CWD_MARKER: &str = "sshdeck-cwd v2";

    #[test]
    fn marker_is_in_snippet_and_script() {
        assert!(CWD_SNIPPET.contains(CWD_MARKER));
        assert!(CWD_SCRIPT.contains(&format!("\"{CWD_MARKER}\"")));
        // Каждая строка фрагмента находится сценарием замены.
        for line in CWD_SNIPPET.lines().filter(|line| !line.is_empty()) {
            assert!(line.contains("sshdeck-cwd") || line.contains("__sshdeck_cwd"), "{line}");
        }
    }

    #[test]
    fn cwd_snippet_is_added_once() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::write(home.path().join(".bashrc"), "alias ll=\"ls -l\"").expect("write");
        assert_eq!(run_cwd_script(home.path(), "/bin/bash"), Some(CwdTracking::Added));
        assert_eq!(run_cwd_script(home.path(), "/bin/bash"), Some(CwdTracking::Present));
        let text = std::fs::read_to_string(home.path().join(".bashrc")).expect("read");
        assert!(text.starts_with("alias ll=\"ls -l\"\n"));
        assert_eq!(text.matches("__sshdeck_cwd()").count(), 1);

        assert_eq!(run_cwd_script(home.path(), "/usr/bin/zsh"), Some(CwdTracking::Added));
        assert!(home.path().join(".zshrc").exists());
        assert_eq!(run_cwd_script(home.path(), "/usr/bin/fish"), Some(CwdTracking::Builtin));
        assert_eq!(run_cwd_script(home.path(), "/bin/dash"), Some(CwdTracking::Unsupported));
    }

    #[test]
    fn old_cwd_snippet_is_replaced() {
        let home = tempfile::tempdir().expect("tempdir");
        let rc = home.path().join(".bashrc");
        std::fs::write(&rc, format!("export A=1\n{CWD_SNIPPET_V1}export B=2\n")).expect("write");
        let mode = std::fs::Permissions::from_mode(0o640);
        std::fs::set_permissions(&rc, mode).expect("chmod");

        assert_eq!(run_cwd_script(home.path(), "/bin/bash"), Some(CwdTracking::Updated));
        let text = std::fs::read_to_string(&rc).expect("read");
        assert!(text.starts_with("export A=1\n"), "{text}");
        assert!(text.contains("export B=2\n"));
        assert!(!text.contains("\"$PWD\"; }"), "старая версия осталась: {text}");
        assert_eq!(text.matches("__sshdeck_cwd()").count(), 1);
        assert_eq!(text.matches("precmd_functions").count(), 1);
        assert!(text.contains(CWD_MARKER));
        assert!(!home.path().join(".bashrc.sshdeck-tmp").exists());
        let meta = std::fs::metadata(&rc).expect("stat");
        assert_eq!(meta.permissions().mode() & 0o777, 0o640);

        assert_eq!(run_cwd_script(home.path(), "/bin/bash"), Some(CwdTracking::Present));
        assert_eq!(std::fs::read_to_string(&rc).expect("read"), text);
    }

    /// Запускает фрагмент в оболочке `shell` в каталоге `dir` и возвращает
    /// то, что он отправил бы терминалу.
    fn snippet_output(shell: &str, dir: &Path) -> Option<Vec<u8>> {
        let rc_dir = tempfile::tempdir().expect("tempdir");
        let rc = rc_dir.path().join("rc");
        std::fs::write(&rc, CWD_SNIPPET).expect("write");
        let script = "source \"$1\"; cd \"$2\" || exit 1; HOSTNAME=srv; HOST=srv; \
                      if [ -n \"${ZSH_VERSION-}\" ]; then for f in $precmd_functions; do $f; done; \
                      else eval \"$PROMPT_COMMAND\"; fi";
        // zsh -f: без файлов настроек пользователя.
        let flags: &[&str] = if shell == "zsh" { &["-f", "-c"] } else { &["-c"] };
        let output = Command::new(shell)
            .args(flags)
            .arg(script)
            .arg(shell)
            .arg(&rc)
            .arg(dir)
            .env("LANG", "C.UTF-8")
            .env_remove("LC_ALL")
            .output()
            .ok()?;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        Some(output.stdout)
    }

    #[test]
    fn cwd_snippet_encodes_the_path_in_bash_and_zsh() {
        use std::os::unix::ffi::OsStrExt;
        let base = tempfile::tempdir().expect("tempdir");
        let dir = base.path().join("my dir").join("папка 'q\"%#");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Имя не в UTF-8 тоже должно дойти побайтно.
        let raw = base.path().join(std::ffi::OsStr::from_bytes(b"latin1-\xe9"));
        std::fs::create_dir(&raw).expect("mkdir");

        for shell in ["bash", "zsh"] {
            for dir in [&dir, &raw] {
                let Some(out) = snippet_output(shell, dir) else {
                    // Оболочки нет в системе.
                    continue;
                };
                let text = String::from_utf8(out).expect("ascii");
                let uri = text
                    .strip_prefix("\x1b]7;")
                    .and_then(|rest| rest.strip_suffix("\x1b\\"))
                    .unwrap_or_else(|| panic!("{shell}: {text:?}"));
                assert!(uri.is_ascii() && !uri.contains(' '), "{shell}: {uri}");
                let reported = parse_cwd_uri(uri).expect("uri");
                assert_eq!(reported.host, "srv");
                assert_eq!(reported.path, dir.as_os_str().as_bytes(), "{shell}: {uri}");
            }
        }
        let out = snippet_output("bash", &dir).expect("bash");
        let text = String::from_utf8(out).expect("ascii");
        assert!(text.ends_with("/my%20dir/%D0%BF%D0%B0%D0%BF%D0%BA%D0%B0%20%27q%22%25%23\x1b\\"), "{text}");
    }

    #[test]
    fn cwd_uri_is_decoded() {
        let dir = parse_cwd_uri("file://web1/home/u/my%20dir/%D0%BF%D0%B0%D0%BF%D0%BA%D0%B0").expect("uri");
        assert_eq!(dir.host, "web1");
        assert_eq!(String::from_utf8(dir.path).expect("utf8"), "/home/u/my dir/папка");
        // Пустой хост, необработанные «%» и «#» остаются как есть.
        let dir = parse_cwd_uri("file:///srv/100%/a#b%zz%4").expect("uri");
        assert_eq!(dir.host, "");
        assert_eq!(dir.path, b"/srv/100%/a#b%zz%4");
        assert_eq!(parse_cwd_uri("file:///x%ff").expect("uri").path, b"/x\xff");
        assert_eq!(parse_cwd_uri("http://h/x"), None);
        assert_eq!(parse_cwd_uri("file://host-only"), None);
    }

    #[test]
    fn host_check() {
        assert!(same_host("web1", "web1"));
        assert!(same_host("Web1", "web1"));
        assert!(same_host("web1", "web1.example.com"));
        assert!(same_host("web1.example.com", "web1"));
        assert!(same_host("web1", ""));
        assert!(same_host("", "db7"));
        assert!(same_host("web1", "localhost"));
        assert!(!same_host("web1", "db7"));
        assert!(!same_host("web1", "web10"));
        assert!(!same_host("web1.prod", "web1.staging"));
    }
}
