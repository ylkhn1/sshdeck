//! Вход без пароля: локальный SSH-ключ и установка его открытой части на
//! сервер.
//!
//! Ключ ставится через уже авторизованное мастер-соединение вкладки, поэтому
//! пароль приложению не нужен и нигде не проходит через него. Все функции
//! блокирующие: вызывать их нужно не из потока GTK.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::remote;
use crate::session::Session;

const SSH_KEYGEN_BIN: &str = "/usr/bin/ssh-keygen";

/// Ключи, которые ssh предлагает серверу сам, без настройки; порядок —
/// порядок предпочтения.
const DEFAULT_KEYS: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];
const NEW_KEY: &str = "id_ed25519";

const MARK_ADDED: &str = "SSHDECK_KEY_ADDED";
const MARK_PRESENT: &str = "SSHDECK_KEY_PRESENT";

/// Сценарий для удалённого `sh`: читает одну строку (открытый ключ) со
/// стандартного ввода и дописывает её в `authorized_keys`, если её там ещё
/// нет. В тексте нет одинарных кавычек: он целиком передаётся в `sh -c '…'`.
const INSTALL_SCRIPT: &str = r#"umask 077; d="$HOME/.ssh"; f="$d/authorized_keys"; mkdir -p "$d" && touch "$f" || exit 1; IFS= read -r key || exit 1; if grep -qxF -e "$key" "$f"; then echo SSHDECK_KEY_PRESENT; else if [ -s "$f" ] && [ -n "$(tail -c 1 "$f")" ]; then echo >> "$f"; fi; printf "%s\n" "$key" >> "$f" && echo SSHDECK_KEY_ADDED; fi"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// Ключ дописан в `authorized_keys`.
    Added,
    /// Такой ключ там уже был.
    AlreadyPresent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub installed: Installed,
    /// Файл открытого ключа, который установлен на сервер.
    pub public_key: PathBuf,
    /// Ключ был создан этим вызовом.
    pub created: bool,
}

/// Открытая часть первого из ключей по умолчанию, у которого есть обе
/// половины.
pub fn existing_key(ssh_dir: &Path) -> Option<PathBuf> {
    DEFAULT_KEYS.iter().find_map(|name| {
        let private = ssh_dir.join(name);
        let public = public_path(&private);
        (private.is_file() && public.is_file()).then_some(public)
    })
}

/// Где появится новый ключ, если подходящего нет.
pub fn new_key_path(ssh_dir: &Path) -> PathBuf {
    ssh_dir.join(NEW_KEY)
}

fn public_path(private: &Path) -> PathBuf {
    let mut name = private.as_os_str().to_owned();
    name.push(".pub");
    PathBuf::from(name)
}

/// Создаёт ключ ed25519 без парольной фразы; возвращает путь открытой части.
pub fn generate_key(ssh_dir: &Path) -> Result<PathBuf, String> {
    if !ssh_dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(ssh_dir)
            .map_err(|err| format!("{}: {err}", ssh_dir.display()))?;
    }
    let private = new_key_path(ssh_dir);
    if private.exists() {
        // Существующий закрытый ключ не перезаписываем ни при каких условиях.
        return Err(tr!(
            "{0} already exists, but there is no .pub file next to it. Restore it with: \
             ssh-keygen -y -f {0} > {0}.pub",
            private.display()
        ));
    }
    let output = Command::new(SSH_KEYGEN_BIN)
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&private)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| tr!("Could not start {}: {}", SSH_KEYGEN_BIN, err))?;
    if !output.status.success() {
        return Err(tr!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(public_path(&private))
}

/// Читает открытый ключ и проверяет, что это одна строка привычного вида.
pub fn read_public(path: &Path) -> Result<String, String> {
    let text = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or_default();
    let mut parts = line.split_whitespace();
    let kind = parts.next().unwrap_or_default();
    let known_kind = ["ssh-", "ecdsa-", "sk-"]
        .iter()
        .any(|prefix| kind.starts_with(prefix));
    if !known_kind || parts.next().is_none() || line.chars().any(char::is_control) {
        return Err(tr!("{}: not an SSH public key", path.display()));
    }
    Ok(line.to_string())
}

/// Удалённая команда для [`Session::command_argv`].
fn remote_command() -> String {
    remote::sh(INSTALL_SCRIPT)
}

/// Дописывает открытый ключ в `~/.ssh/authorized_keys` на сервере через
/// мастер-соединение сессии.
pub fn install(session: &Session, public_line: &str) -> Result<Installed, String> {
    let output = remote::run(session, &remote_command(), &format!("{public_line}\n"))?;
    parse_reply(&output.stdout, &output.stderr)
}

fn parse_reply(stdout: &str, stderr: &str) -> Result<Installed, String> {
    if stdout.lines().any(|line| line.trim() == MARK_ADDED) {
        Ok(Installed::Added)
    } else if stdout.lines().any(|line| line.trim() == MARK_PRESENT) {
        Ok(Installed::AlreadyPresent)
    } else {
        let detail = stderr.trim();
        Err(if detail.is_empty() {
            tr!("The server did not confirm that the key was installed").to_string()
        } else {
            tr!("Could not install the key on the server: {}", detail)
        })
    }
}

/// Полный путь: взять существующий ключ или создать новый и установить его
/// на сервер.
pub fn setup(session: &Session, ssh_dir: &Path) -> Result<Outcome, String> {
    let (public_key, created) = match existing_key(ssh_dir) {
        Some(path) => (path, false),
        None => (generate_key(ssh_dir)?, true),
    };
    let line = read_public(&public_key)?;
    let installed = install(session, &line)?;
    Ok(Outcome {
        installed,
        public_key,
        created,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa a@test";
    const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb b@test";

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o777)
            .unwrap_or(0)
    }

    /// Выполняет удалённый сценарий локально, подменив домашний каталог.
    fn run_script(home: &Path, key: &str) -> String {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(INSTALL_SCRIPT)
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("sh");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(format!("{key}\n").as_bytes())
            .expect("write");
        let output = child.wait_with_output().expect("wait");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn script_has_no_single_quotes() {
        assert!(!INSTALL_SCRIPT.contains('\''));
        assert!(remote_command().starts_with("sh -c '"));
    }

    #[test]
    fn script_adds_key_once() {
        let home = tempfile::tempdir().expect("tempdir");
        assert_eq!(run_script(home.path(), KEY_A), MARK_ADDED);
        assert_eq!(run_script(home.path(), KEY_A), MARK_PRESENT);
        assert_eq!(run_script(home.path(), KEY_B), MARK_ADDED);

        let file = home.path().join(".ssh/authorized_keys");
        let text = fs::read_to_string(&file).expect("read");
        assert_eq!(text, format!("{KEY_A}\n{KEY_B}\n"));
        assert_eq!(mode_of(&file), 0o600);
        assert_eq!(mode_of(&home.path().join(".ssh")), 0o700);
    }

    #[test]
    fn script_keeps_existing_keys_without_trailing_newline() {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = home.path().join(".ssh");
        fs::create_dir(&dir).expect("mkdir");
        fs::write(dir.join("authorized_keys"), KEY_A).expect("write");
        assert_eq!(run_script(home.path(), KEY_B), MARK_ADDED);
        let text = fs::read_to_string(dir.join("authorized_keys")).expect("read");
        assert_eq!(text, format!("{KEY_A}\n{KEY_B}\n"));
    }

    #[test]
    fn replies_are_recognised() {
        assert_eq!(parse_reply("SSHDECK_KEY_ADDED\n", ""), Ok(Installed::Added));
        assert_eq!(
            parse_reply("motd\nSSHDECK_KEY_PRESENT\n", "warning"),
            Ok(Installed::AlreadyPresent)
        );
        assert!(parse_reply("", "Permission denied").is_err());
        assert!(parse_reply("", "").is_err());
    }

    #[test]
    fn public_key_is_validated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = dir.path().join("good.pub");
        fs::write(&good, format!("\n{KEY_A}\n")).expect("write");
        assert_eq!(read_public(&good), Ok(KEY_A.to_string()));

        let bad = dir.path().join("bad.pub");
        fs::write(&bad, "-----BEGIN OPENSSH PRIVATE KEY-----\n").expect("write");
        assert!(read_public(&bad).is_err());
        assert!(read_public(&dir.path().join("missing.pub")).is_err());
    }

    #[test]
    fn existing_key_needs_both_halves() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(existing_key(dir.path()), None);
        fs::write(dir.path().join("id_rsa"), "x").expect("write");
        assert_eq!(existing_key(dir.path()), None);
        fs::write(dir.path().join("id_rsa.pub"), "x").expect("write");
        assert_eq!(existing_key(dir.path()), Some(dir.path().join("id_rsa.pub")));
        // ed25519 предпочтительнее rsa.
        fs::write(dir.path().join("id_ed25519"), "x").expect("write");
        fs::write(dir.path().join("id_ed25519.pub"), "x").expect("write");
        assert_eq!(
            existing_key(dir.path()),
            Some(dir.path().join("id_ed25519.pub"))
        );
    }

    #[test]
    fn key_is_generated_and_never_overwritten() {
        let home = tempfile::tempdir().expect("tempdir");
        let ssh_dir = home.path().join(".ssh");
        let public = generate_key(&ssh_dir).expect("generate");
        assert_eq!(public, ssh_dir.join("id_ed25519.pub"));
        assert!(read_public(&public).is_ok());
        assert_eq!(mode_of(&ssh_dir), 0o700);
        assert_eq!(mode_of(&ssh_dir.join("id_ed25519")), 0o600);
        assert_eq!(existing_key(&ssh_dir), Some(public.clone()));

        // Закрытый ключ без .pub не затирается.
        fs::remove_file(&public).expect("remove pub");
        let before = fs::read(ssh_dir.join("id_ed25519")).expect("read");
        assert!(generate_key(&ssh_dir).is_err());
        assert_eq!(fs::read(ssh_dir.join("id_ed25519")).expect("read"), before);
    }
}
