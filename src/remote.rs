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
    debug_assert!(!script.contains('\''), "сценарий содержит одинарную кавычку");
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
        return Err(
            "Нет активного соединения. Сначала войдите на сервер в терминале этой вкладки."
                .to_string(),
        );
    }
    let argv = session.command_argv(command);
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Не удалось запустить ssh: {err}"))?;
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
        .map_err(|err| format!("Не удалось запустить ssh: {err}"))?;
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
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

// ------------------------------------------------- слежение за каталогом

const CWD_ADDED: &str = "SSHDECK_CWD_ADDED";
const CWD_PRESENT: &str = "SSHDECK_CWD_PRESENT";
const CWD_BUILTIN: &str = "SSHDECK_CWD_BUILTIN";
const CWD_UNSUPPORTED: &str = "SSHDECK_CWD_UNSUPPORTED";

/// Дописывает в файл настроек оболочки то, что пришло на стандартный ввод,
/// если там ещё нет метки `sshdeck-cwd`. Без одинарных кавычек.
const CWD_SCRIPT: &str = r#"case "$(basename "${SHELL:-sh}")" in bash) f=.bashrc;; zsh) f=.zshrc;; fish) echo SSHDECK_CWD_BUILTIN; exit 0;; *) echo SSHDECK_CWD_UNSUPPORTED; exit 0;; esac; f="$HOME/$f"; if grep -qF sshdeck-cwd "$f" 2>/dev/null; then echo SSHDECK_CWD_PRESENT; else cat >> "$f" && echo SSHDECK_CWD_ADDED; fi"#;

/// Фрагмент для `.bashrc`/`.zshrc`: перед каждым приглашением сообщает
/// терминалу текущий каталог (последовательность OSC 7).
const CWD_SNIPPET: &str = r#"
# sshdeck-cwd: report the current directory to the terminal (OSC 7)
__sshdeck_cwd() { printf '\033]7;file://%s%s\033\\' "${HOSTNAME:-$HOST}" "$PWD"; }
if [ -n "$ZSH_VERSION" ]; then precmd_functions+=(__sshdeck_cwd); else PROMPT_COMMAND="__sshdeck_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"; fi
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CwdTracking {
    /// Фрагмент дописан; заработает в новых сеансах оболочки.
    Added,
    /// Фрагмент уже был на месте.
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
            "Сервер не подтвердил изменение настроек оболочки".to_string()
        } else {
            format!("Не удалось изменить настройки оболочки: {detail}")
        }
    })
}

fn parse_cwd_reply(stdout: &str) -> Option<CwdTracking> {
    stdout.lines().find_map(|line| match line.trim() {
        CWD_ADDED => Some(CwdTracking::Added),
        CWD_PRESENT => Some(CwdTracking::Present),
        CWD_BUILTIN => Some(CwdTracking::Builtin),
        CWD_UNSUPPORTED => Some(CwdTracking::Unsupported),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;
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
    fn cwd_snippet_reports_directory_in_bash() {
        if !Path::new("/bin/bash").exists() && !Path::new("/usr/bin/bash").exists() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let rc = dir.path().join("rc");
        std::fs::write(&rc, CWD_SNIPPET).expect("write");
        let output = Command::new("bash")
            .arg("--norc")
            .arg("-c")
            .arg("source \"$1\"; cd /tmp; HOSTNAME=srv; eval \"$PROMPT_COMMAND\"")
            .arg("bash")
            .arg(&rc)
            .output()
            .expect("bash");
        assert_eq!(output.stdout, b"\x1b]7;file://srv/tmp\x1b\\");
    }
}
