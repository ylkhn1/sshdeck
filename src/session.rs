//! Параметры ssh-сессии одной вкладки: управляющий сокет и командные строки
//! для системного `ssh`.
//!
//! Терминал запускает `ssh` с `ControlMaster=auto`, а SFTP-панель и служебные
//! команды (`-O check`, `-O exit`) ходят через тот же сокет. Модуль ничего не
//! знает ни о GTK, ни о tokio — только собирает аргументы.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Системный клиент OpenSSH.
pub const SSH_BIN: &str = "/usr/bin/ssh";

/// Переменная окружения с альтернативным путём к ssh-конфигу (передаётся
/// как `ssh -F`). Полезна для отладки и отдельных наборов хостов.
pub const CONFIG_ENV: &str = "SSHDECK_SSH_CONFIG";

/// Сколько секунд мастер-соединение живёт без клиентов.
///
/// `ControlPersist` нужен, чтобы `ssh` в терминале завершался сразу после
/// выхода из шелла: без него мастер ждёт, пока отключится SFTP-клиент, и
/// вкладка «зависает». Небольшое значение гарантирует, что после аварийного
/// завершения приложения фоновый мастер сам исчезнет.
const PERSIST_SECS: u32 = 10;

/// Как часто ssh в терминале проверяет связь с сервером при простое.
const KEEPALIVE_SECS: u32 = 30;

/// Каталог приложения для управляющих сокетов: `$XDG_RUNTIME_DIR/sshdeck`,
/// доступный только владельцу.
pub fn runtime_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "переменная XDG_RUNTIME_DIR не задана",
            )
        })?;
    let dir = base.join("sshdeck");
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Создаёт каталог с правами 0700 (или исправляет права существующего).
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        }
        Err(err) => Err(err),
    }
}

/// Удаляет из каталога приложения сокеты и временные каталоги, оставшиеся
/// от процессов, которых уже нет (после аварийного завершения). Записи
/// живых процессов, включая другие запущенные экземпляры, не трогаются.
pub fn cleanup_stale(runtime_dir: &Path) {
    let Ok(entries) = fs::read_dir(runtime_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = owner_pid(&name.to_string_lossy()) else {
            continue;
        };
        if Path::new("/proc").join(pid.to_string()).exists() {
            continue;
        }
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let _ = if is_dir {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
    }
}

/// Идентификатор процесса-владельца из имени `<pid>-<n>.sock`,
/// `files-<pid>-<n>` или `db-<pid>-<n>` (каталог туннеля к базе данных);
/// для посторонних имён — `None`.
fn owner_pid(name: &str) -> Option<u32> {
    let rest = match name
        .strip_prefix("files-")
        .or_else(|| name.strip_prefix("db-"))
    {
        Some(rest) => rest,
        None => name.strip_suffix(".sock")?,
    };
    let (pid, tab) = rest.split_once('-')?;
    tab.parse::<u32>().ok()?;
    pid.parse().ok()
}

/// Путь к ssh-конфигу из [`CONFIG_ENV`], если переменная задана.
pub fn config_override() -> Option<PathBuf> {
    std::env::var_os(CONFIG_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Проверяет и нормализует адрес, введённый вручную.
///
/// Понимает `user@host`, псевдоним, `ssh://user@host:port`, а также порт в
/// привычных записях: `user@host -p 2222`, `-p 2222 user@host`,
/// `user@host:2222` и вставленную целиком команду `ssh user@host -p 2222`.
/// Адрес с портом возвращается в виде `ssh://user@host:port`.
///
/// Главное — не дать строке превратиться в опцию ssh: кроме `-p <порт>`
/// никакие параметры не принимаются, а сам адрес не может начинаться с `-`
/// и содержать пробелы или управляющие символы.
pub fn validate_target(input: &str) -> Result<String, String> {
    const EMPTY: &str = "Введите адрес в виде user@host";

    if input.trim().chars().any(char::is_control) {
        return Err("Адрес содержит недопустимые символы".to_string());
    }
    let mut tokens: Vec<&str> = input.split_whitespace().collect();
    if tokens.len() > 1 && tokens[0] == "ssh" {
        tokens.remove(0);
    }

    let mut destination: Option<&str> = None;
    let mut port: Option<u16> = None;
    let mut tokens = tokens.into_iter();
    while let Some(token) = tokens.next() {
        if let Some(inline) = token.strip_prefix("-p") {
            let value = if inline.is_empty() {
                tokens
                    .next()
                    .ok_or_else(|| "После -p нужно указать порт".to_string())?
            } else {
                inline
            };
            if port.is_some() {
                return Err("Порт указан дважды".to_string());
            }
            port = Some(parse_port(value)?);
        } else if token.starts_with('-') {
            return Err(format!(
                "Параметр «{token}» не поддерживается: можно указать только -p <порт>"
            ));
        } else if destination.is_some() {
            return Err("Адрес не должен содержать пробелов".to_string());
        } else {
            destination = Some(token);
        }
    }
    let destination = destination.ok_or_else(|| EMPTY.to_string())?;

    let (bare, is_uri) = match destination.strip_prefix("ssh://") {
        Some(rest) => (rest, true),
        None => (destination, false),
    };
    if bare.is_empty() {
        return Err(EMPTY.to_string());
    }

    // `user@host:2222` без схемы. Адрес IPv6 без скобок содержит несколько
    // двоеточий и сюда не попадает.
    let (bare, port) = match bare.rsplit_once(':') {
        Some((host, digits))
            if !is_uri
                && !host.is_empty()
                && !host.contains(':')
                && !digits.is_empty()
                && digits.chars().all(|c| c.is_ascii_digit()) =>
        {
            if port.is_some() {
                return Err("Порт указан дважды".to_string());
            }
            (host, Some(parse_port(digits)?))
        }
        _ => (bare, port),
    };

    let Some(port) = port else {
        return Ok(destination.to_string());
    };
    if is_uri && uri_has_port(bare) {
        return Err("Порт указан дважды".to_string());
    }
    let (user, host) = match bare.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, bare),
    };
    if host.is_empty() {
        return Err(EMPTY.to_string());
    }
    // В URI адрес IPv6 записывается в квадратных скобках.
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(match user {
        Some(user) => format!("ssh://{user}@{host}:{port}"),
        None => format!("ssh://{host}:{port}"),
    })
}

fn parse_port(value: &str) -> Result<u16, String> {
    match value.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(format!("Неверный порт: «{value}» (нужно число от 1 до 65535)")),
    }
}

/// Есть ли порт в адресе вида `user@host:port` или `user@[v6]:port`.
fn uri_has_port(bare: &str) -> bool {
    let host = bare.rsplit_once('@').map_or(bare, |(_, host)| host);
    match host.rsplit_once(':') {
        Some((_, tail)) => !tail.is_empty() && !tail.contains(']'),
        None => false,
    }
}

/// Сессия вкладки: куда подключаемся и через какой сокет.
#[derive(Debug, Clone)]
pub struct Session {
    target: String,
    control_path: PathBuf,
    config: Option<PathBuf>,
}

impl Session {
    /// `target` должен быть уже проверен [`validate_target`]. Имя сокета
    /// уникально для процесса и вкладки, поэтому каждая вкладка держит
    /// собственное мастер-соединение.
    pub fn new(target: &str, runtime_dir: &Path, tab_id: u32, config: Option<PathBuf>) -> Self {
        let control_path = runtime_dir.join(format!("{}-{tab_id}.sock", std::process::id()));
        // Сокет с таким именем мог остаться только от упавшего процесса с тем
        // же pid; живого мастера за ним нет.
        let _ = fs::remove_file(&control_path);
        Self {
            target: target.to_string(),
            control_path,
            config,
        }
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    /// Путь сокета для ssh: `%` экранируется, иначе ssh примет его за токен.
    fn control_arg(&self) -> String {
        self.control_path.to_string_lossy().replace('%', "%%")
    }

    fn base(&self) -> Vec<String> {
        let mut argv = vec![SSH_BIN.to_string()];
        if let Some(config) = &self.config {
            argv.push("-F".to_string());
            argv.push(config.to_string_lossy().into_owned());
        }
        argv
    }

    /// Команда для терминала: интерактивный ssh, который поднимает мастер.
    pub fn terminal_argv(&self) -> Vec<String> {
        let mut argv = self.base();
        argv.extend([
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            format!("ControlPath={}", self.control_arg()),
            "-o".to_string(),
            format!("ControlPersist={PERSIST_SECS}"),
            // Не даёт простаивающей сессии оборваться на NAT и файрволах.
            "-o".to_string(),
            format!("ServerAliveInterval={KEEPALIVE_SECS}"),
            "--".to_string(),
            self.target.clone(),
        ]);
        argv
    }

    fn control_command(&self, op: &str) -> Vec<String> {
        let mut argv = self.base();
        argv.extend([
            "-S".to_string(),
            self.control_arg(),
            "-O".to_string(),
            op.to_string(),
            "--".to_string(),
            self.target.clone(),
        ]);
        argv
    }

    /// `ssh -S <сокет> -O check <хост>`: жив ли мастер.
    pub fn check_argv(&self) -> Vec<String> {
        self.control_command("check")
    }

    /// `ssh -S <сокет> -O exit <хост>`: завершить мастер.
    pub fn exit_argv(&self) -> Vec<String> {
        self.control_command("exit")
    }

    /// SFTP-подсистема через существующий мастер. `BatchMode` и
    /// `ControlMaster=no` не дают ssh начать собственную аутентификацию,
    /// если мастера вдруг нет: он просто завершится с ошибкой. Настройки
    /// хоста, осмысленные только для интерактивной сессии (`RemoteCommand`,
    /// tty, проброс портов), для этого вызова отключаются.
    pub fn sftp_argv(&self) -> Vec<String> {
        let mut argv = self.base();
        argv.extend([
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ControlMaster=no".to_string(),
            "-o".to_string(),
            "RemoteCommand=none".to_string(),
            "-o".to_string(),
            "RequestTTY=no".to_string(),
            "-o".to_string(),
            "ClearAllForwardings=yes".to_string(),
            "-S".to_string(),
            self.control_arg(),
            "-s".to_string(),
            "--".to_string(),
            self.target.clone(),
            "sftp".to_string(),
        ]);
        argv
    }

    /// Удалённая команда через существующий мастер, без tty и без
    /// собственной аутентификации (те же ограничения, что у SFTP-вызова).
    /// `remote` передаётся удалённой оболочке как есть.
    pub fn command_argv(&self, remote: &str) -> Vec<String> {
        let mut argv = self.base();
        argv.extend([
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ControlMaster=no".to_string(),
            "-o".to_string(),
            "RemoteCommand=none".to_string(),
            "-o".to_string(),
            "RequestTTY=no".to_string(),
            "-o".to_string(),
            "ClearAllForwardings=yes".to_string(),
            "-S".to_string(),
            self.control_arg(),
            "--".to_string(),
            self.target.clone(),
            remote.to_string(),
        ]);
        argv
    }

    /// Просит мастер завершиться, не дожидаясь результата. Вызывается при
    /// закрытии вкладки; ошибки не важны — мастера может уже не быть.
    pub fn close_master(&self) {
        if !self.control_path.exists() {
            return;
        }
        let argv = self.exit_argv();
        let spawned = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = spawned {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(config: Option<&str>) -> Session {
        Session::new(
            "user@example",
            Path::new("/nonexistent-run/sshdeck"),
            7,
            config.map(PathBuf::from),
        )
    }

    #[test]
    fn owner_pid_parsing() {
        assert_eq!(owner_pid("1234-1.sock"), Some(1234));
        assert_eq!(owner_pid("files-1234-7"), Some(1234));
        assert_eq!(owner_pid("db-1234-3"), Some(1234));
        assert_eq!(owner_pid("files-1234"), None);
        assert_eq!(owner_pid("1234.sock"), None);
        assert_eq!(owner_pid("abc-1.sock"), None);
        assert_eq!(owner_pid("1234-x.sock"), None);
        assert_eq!(owner_pid("unrelated"), None);
    }

    #[test]
    fn stale_entries_are_removed_and_live_ones_kept() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let me = std::process::id();
        // PID выше pid_max ядра: такого процесса заведомо нет.
        let dead = u32::MAX - 1;
        let path = |name: String| dir.path().join(name);
        fs::write(path(format!("{dead}-1.sock")), "").expect("write");
        fs::create_dir_all(path(format!("files-{dead}-1")).join("edit-1")).expect("mkdir");
        fs::write(path(format!("{me}-1.sock")), "").expect("write");
        fs::create_dir(path(format!("files-{me}-1"))).expect("mkdir");
        fs::write(path("unrelated".to_string()), "").expect("write");

        cleanup_stale(dir.path());

        assert!(!path(format!("{dead}-1.sock")).exists());
        assert!(!path(format!("files-{dead}-1")).exists());
        assert!(path(format!("{me}-1.sock")).exists());
        assert!(path(format!("files-{me}-1")).exists());
        assert!(path("unrelated".to_string()).exists());
    }

    #[test]
    fn target_validation() {
        assert_eq!(validate_target("  root@host  "), Ok("root@host".to_string()));
        assert_eq!(
            validate_target("ssh://u@h:2222"),
            Ok("ssh://u@h:2222".to_string())
        );
        assert!(validate_target("").is_err());
        assert!(validate_target("   ").is_err());
        assert!(validate_target("-oProxyCommand=evil").is_err());
        assert!(validate_target("host extra").is_err());
        assert!(validate_target("host\tx").is_err());
    }

    #[test]
    fn target_with_port() {
        let expected = Ok("ssh://t@82.200.129.153:18122".to_string());
        assert_eq!(validate_target("t@82.200.129.153 -p 18122"), expected);
        assert_eq!(validate_target("-p 18122 t@82.200.129.153"), expected);
        assert_eq!(validate_target("t@82.200.129.153 -p18122"), expected);
        assert_eq!(validate_target("t@82.200.129.153:18122"), expected);
        assert_eq!(validate_target("ssh t@82.200.129.153 -p 18122"), expected);
        assert_eq!(validate_target("ssh://t@82.200.129.153 -p 18122"), expected);
        assert_eq!(
            validate_target("myhost -p 2222"),
            Ok("ssh://myhost:2222".to_string())
        );
        assert_eq!(
            validate_target("u@::1 -p 2222"),
            Ok("ssh://u@[::1]:2222".to_string())
        );
        // Голый IPv6 не принимается за «хост:порт».
        assert_eq!(validate_target("u@fe80::1"), Ok("u@fe80::1".to_string()));
        // Хост с именем ssh остаётся хостом.
        assert_eq!(validate_target("ssh"), Ok("ssh".to_string()));
    }

    #[test]
    fn target_with_bad_port_or_options() {
        assert!(validate_target("host -p").is_err());
        assert!(validate_target("host -p 0").is_err());
        assert!(validate_target("host -p 70000").is_err());
        assert!(validate_target("host -p abc").is_err());
        assert!(validate_target("host:22 -p 23").is_err());
        assert!(validate_target("ssh://host:22 -p 23").is_err());
        assert!(validate_target("host -p 22 -p 23").is_err());
        assert!(validate_target("host -oProxyCommand=evil").is_err());
        assert!(validate_target("host -L 80:a:80").is_err());
        assert!(validate_target("-p 22").is_err());
    }

    #[test]
    fn terminal_command_sets_up_master() {
        let s = session(None);
        let argv = s.terminal_argv();
        let sock = format!(
            "ControlPath=/nonexistent-run/sshdeck/{}-7.sock",
            std::process::id()
        );
        assert_eq!(argv[0], SSH_BIN);
        assert!(argv.contains(&"ControlMaster=auto".to_string()));
        assert!(argv.contains(&sock));
        assert_eq!(&argv[argv.len() - 2..], ["--", "user@example"]);
    }

    #[test]
    fn config_override_is_passed_to_every_command() {
        let s = session(Some("/tmp/alt_config"));
        for argv in [
            s.terminal_argv(),
            s.check_argv(),
            s.exit_argv(),
            s.sftp_argv(),
        ] {
            assert_eq!(&argv[1..3], ["-F", "/tmp/alt_config"]);
        }
    }

    #[test]
    fn sftp_command_never_authenticates_itself() {
        let argv = session(None).sftp_argv();
        assert!(argv.contains(&"BatchMode=yes".to_string()));
        assert!(argv.contains(&"ControlMaster=no".to_string()));
        assert!(argv.contains(&"RemoteCommand=none".to_string()));
        assert_eq!(&argv[argv.len() - 4..], ["-s", "--", "user@example", "sftp"]);
    }

    #[test]
    fn control_commands() {
        let s = session(None);
        let check = s.check_argv();
        assert_eq!(&check[check.len() - 4..], ["-O", "check", "--", "user@example"]);
        let exit = s.exit_argv();
        assert_eq!(&exit[exit.len() - 4..], ["-O", "exit", "--", "user@example"]);
    }
}
