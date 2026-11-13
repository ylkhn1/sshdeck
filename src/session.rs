//! Параметры ssh-сессии вкладки: управляющий сокет мастер-соединения и
//! командные строки для системного `ssh`.
//!
//! Терминал запускает `ssh` с `ControlMaster=auto`, а SFTP-панель и служебные
//! команды (`-O check`, `-O exit`) ходят через тот же сокет. Модуль ничего не
//! знает ни о GTK, ни о tokio — только собирает аргументы и считает, сколько
//! вкладок пользуется каждым мастером.
//!
//! # Общий мастер
//!
//! Вкладки с одним и тем же адресом назначения делят одно мастер-соединение
//! ([`Session::shared`]): сокет называется по хэшу итоговых `user`, `hostname`
//! и `port` (из `ssh -G`) и пути к конфигу. Первый `ssh` с
//! `ControlMaster=auto` входит на сервер и становится мастером; из-за
//! `ControlPersist` мастер сразу уходит в фон, а сам терминал становится его
//! клиентом. Поэтому выход из шелла в первой вкладке мастер не гасит: он
//! живёт, пока у него есть клиенты (терминалы и SFTP других вкладок), и ещё
//! [`PERSIST_SECS`] секунд после последнего. Следующие вкладки с тем же
//! адресом подключаются к сокету как клиенты и пароль не спрашивают. Если
//! мастера нет (разрыв связи), первый же `ssh` любой вкладки поднимает новый.
//!
//! Каждая [`Session`] держит «аренду» сокета: счётчик вкладок ведётся в
//! процессе, и `ssh -O exit` отправляется, только когда сокет отпускает
//! последняя вкладка ([`Session::close_master`]). В имени сокета есть pid
//! процесса — по нему [`cleanup_stale`] убирает сокеты упавших запусков, а
//! разные экземпляры приложения не гасят мастера друг друга.
//!
//! Для другого кода, которому нужен мастер вкладки (туннели, проброс
//! портов): [`Session::control_path`] — путь к сокету, [`Session::control_argv`]
//! — команда `ssh -S <сокет> -O <операция>` (например, `forward`/`cancel` с
//! `-L`/`-R`), `remote::master_alive` — жив ли мастер. Проброс, добавленный
//! через `-O forward`, живёт в общем мастере, виден всем вкладкам с этим
//! адресом и пропадает вместе с мастером; снимать его нужно `-O cancel`.
//! Для таких команд лучше [`Session::mux_argv`]: он не читает ssh-конфиг, и
//! `-O cancel` не снимает заодно `LocalForward` из конфига хоста (см.
//! `crate::forward`).

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use crate::ssh_config::{self, Resolved};

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
pub const PERSIST_SECS: u32 = 10;

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
                tr!("XDG_RUNTIME_DIR is not set"),
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

/// Идентификатор процесса-владельца из имени `<pid>-<n>.sock` (сокет
/// отдельной сессии), `<pid>-m<хэш>.sock` (общий мастер), `files-<pid>-<n>`
/// или `db-<pid>-<n>` (каталог туннеля к базе данных); для посторонних
/// имён — `None`.
fn owner_pid(name: &str) -> Option<u32> {
    let (rest, socket) = match name
        .strip_prefix("files-")
        .or_else(|| name.strip_prefix("db-"))
    {
        Some(rest) => (rest, false),
        None => (name.strip_suffix(".sock")?, true),
    };
    let (pid, id) = rest.split_once('-')?;
    let shared = socket
        && id
            .strip_prefix('m')
            .is_some_and(|hash| !hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit()));
    if !shared {
        id.parse::<u32>().ok()?;
    }
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
    let empty = || tr!("Enter an address like user@host").to_string();

    if input.trim().chars().any(char::is_control) {
        return Err(tr!("The address contains invalid characters").to_string());
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
                    .ok_or_else(|| tr!("A port number must follow -p").to_string())?
            } else {
                inline
            };
            if port.is_some() {
                return Err(tr!("The port is specified twice").to_string());
            }
            port = Some(parse_port(value)?);
        } else if token.starts_with('-') {
            return Err(tr!(
                "Option “{}” is not supported: only -p <port> is allowed",
                token
            ));
        } else if destination.is_some() {
            return Err(tr!("The address must not contain spaces").to_string());
        } else {
            destination = Some(token);
        }
    }
    let destination = destination.ok_or_else(empty)?;

    let (bare, is_uri) = match destination.strip_prefix("ssh://") {
        Some(rest) => (rest, true),
        None => (destination, false),
    };
    if bare.is_empty() {
        return Err(empty());
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
                return Err(tr!("The port is specified twice").to_string());
            }
            (host, Some(parse_port(digits)?))
        }
        _ => (bare, port),
    };

    let Some(port) = port else {
        return Ok(destination.to_string());
    };
    if is_uri && uri_has_port(bare) {
        return Err(tr!("The port is specified twice").to_string());
    }
    let (user, host) = match bare.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, bare),
    };
    if host.is_empty() {
        return Err(empty());
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
        _ => Err(tr!("Invalid port “{}” (must be a number from 1 to 65535)", value)),
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

// ------------------------------------------------------- общие мастера

/// Сколько сессий процесса пользуется каждым сокетом.
static MASTERS: LazyLock<Mutex<HashMap<PathBuf, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Сколько сессий процесса сейчас держат сокет `control_path`.
#[cfg(test)]
fn master_users(control_path: &Path) -> usize {
    MASTERS
        .lock()
        .map_or(0, |masters| masters.get(control_path).copied().unwrap_or(0))
}

/// Регистрирует ещё одного пользователя сокета. Первый пользователь в
/// процессе убирает оставшийся файл сокета, если за ним никого нет: с таким
/// файлом `ssh` не смог бы стать мастером.
fn acquire(control_path: &Path) {
    let mut masters = MASTERS.lock().unwrap_or_else(|e| e.into_inner());
    let users = masters.entry(control_path.to_path_buf()).or_insert(0);
    *users += 1;
    if *users == 1 && control_path.exists() && UnixStream::connect(control_path).is_err() {
        let _ = fs::remove_file(control_path);
    }
}

/// Снимает регистрацию; `true`, если это был последний пользователь.
fn release(control_path: &Path) -> bool {
    let mut masters = MASTERS.lock().unwrap_or_else(|e| e.into_inner());
    match masters.get_mut(control_path) {
        Some(users) if *users > 1 => {
            *users -= 1;
            false
        }
        Some(_) => {
            masters.remove(control_path);
            true
        }
        None => false,
    }
}

/// Ключ общего мастера: FNV-1a от итоговых `user`, `hostname`, `port` и
/// пути к конфигу. Если `ssh -G` не ответил, ключом служит сам адрес —
/// тогда общими будут только вкладки с одинаково записанным адресом.
fn master_key(resolved: Option<&Resolved>, target: &str, config: Option<&Path>) -> String {
    let mut parts: Vec<&str> = match resolved {
        Some(resolved) => ["user", "hostname", "port"]
            .iter()
            .map(|key| resolved.get(key).unwrap_or_default())
            .collect(),
        None => vec![target],
    };
    let config = config.map(|path| path.to_string_lossy().into_owned());
    parts.push(config.as_deref().unwrap_or_default());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in parts.join("\\0").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Доля сессии в сокете. Общая для всех клонов [`Session`]: отпускается
/// один раз — явным [`Session::close_master`] или когда исчезнет последний
/// клон.
#[derive(Debug)]
struct Lease {
    control_path: PathBuf,
    exit_argv: Vec<String>,
    released: AtomicBool,
}

impl Lease {
    fn release(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        if release(&self.control_path) && self.control_path.exists() {
            run_detached(&self.exit_argv);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.release();
    }
}

/// Запускает команду, не дожидаясь результата.
fn run_detached(argv: &[String]) {
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

/// `ssh [-F конфиг]`.
fn ssh_base(config: Option<&Path>) -> Vec<String> {
    let mut argv = vec![SSH_BIN.to_string()];
    if let Some(config) = config {
        argv.push("-F".to_string());
        argv.push(config.to_string_lossy().into_owned());
    }
    argv
}

/// Путь сокета для ssh: `%` экранируется, иначе ssh примет его за токен.
fn control_arg(control_path: &Path) -> String {
    control_path.to_string_lossy().replace('%', "%%")
}

/// `ssh [-F конфиг] -S <сокет> <options…> -O <op> -- <хост>`.
fn control_command(
    config: Option<&Path>,
    control_path: &Path,
    target: &str,
    op: &str,
    options: &[&str],
) -> Vec<String> {
    let mut argv = ssh_base(config);
    argv.extend(["-S".to_string(), control_arg(control_path)]);
    argv.extend(options.iter().map(|option| option.to_string()));
    argv.extend([
        "-O".to_string(),
        op.to_string(),
        "--".to_string(),
        target.to_string(),
    ]);
    argv
}

/// Сессия вкладки: куда подключаемся и через какой сокет.
#[derive(Debug, Clone)]
pub struct Session {
    target: String,
    control_path: PathBuf,
    config: Option<PathBuf>,
    lease: Arc<Lease>,
}

impl Session {
    /// Сессия с собственным сокетом `<pid>-<tab_id>.sock`, которым никто
    /// больше не пользуется (тесты и проверки на живом сервере). `target`
    /// должен быть уже проверен [`validate_target`].
    #[cfg(test)]
    pub fn new(target: &str, runtime_dir: &Path, tab_id: u32, config: Option<PathBuf>) -> Self {
        let control_path = runtime_dir.join(format!("{}-{tab_id}.sock", std::process::id()));
        Self::with_control_path(target, control_path, config)
    }

    /// Сессия вкладки на общем мастере: все вкладки процесса с тем же
    /// итоговым адресом (пользователь, хост, порт) и тем же конфигом
    /// получают один и тот же сокет. Блокирующая: спрашивает `ssh -G`
    /// (обычно это миллисекунды).
    pub fn shared(target: &str, runtime_dir: &Path, config: Option<PathBuf>) -> Self {
        let resolved = ssh_config::resolve(target, config.as_deref()).ok();
        let key = master_key(resolved.as_ref(), target, config.as_deref());
        let control_path = runtime_dir.join(format!("{}-m{key}.sock", std::process::id()));
        Self::with_control_path(target, control_path, config)
    }

    fn with_control_path(target: &str, control_path: PathBuf, config: Option<PathBuf>) -> Self {
        acquire(&control_path);
        let lease = Lease {
            control_path: control_path.clone(),
            exit_argv: control_command(config.as_deref(), &control_path, target, "exit", &[]),
            released: AtomicBool::new(false),
        };
        Self {
            target: target.to_string(),
            control_path,
            config,
            lease: Arc::new(lease),
        }
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    /// Путь к управляющему сокету мастер-соединения.
    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    /// Альтернативный ssh-конфиг (`ssh -F`), если он задан.
    pub fn config(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    /// `ssh [-F конфиг]` — начало любой команды этой сессии.
    pub fn base(&self) -> Vec<String> {
        ssh_base(self.config.as_deref())
    }

    /// Путь сокета для ssh.
    fn control_arg(&self) -> String {
        control_arg(&self.control_path)
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

    /// Команда управления мастером: `ssh [-F конфиг] -S <сокет> <options…>
    /// -O <op> -- <хост>`. `op` — `check`, `exit`, `forward`, `cancel` и т. п.
    /// (см. `ssh -O`); `options` — например, `["-L", "8080:localhost:80"]`
    /// для `forward`/`cancel`.
    pub fn control_argv(&self, op: &str, options: &[&str]) -> Vec<String> {
        control_command(self.config.as_deref(), &self.control_path, &self.target, op, options)
    }

    /// Как [`Session::control_argv`], но без ssh-конфига (`-F /dev/null`).
    ///
    /// Клиент `ssh -O forward` и `ssh -O cancel` отправляет мастеру не только
    /// пробросы из командной строки, но и все `LocalForward`/`RemoteForward`/
    /// `DynamicForward` из конфига этого хоста: `-O cancel` снял бы заодно и
    /// их. Для команды через сокет конфиг не нужен — сокет указан явно, а
    /// сама команда не входит на сервер.
    pub fn mux_argv(&self, op: &str, options: &[&str]) -> Vec<String> {
        control_command(Some(Path::new("/dev/null")), &self.control_path, &self.target, op, options)
    }

    /// `ssh -S <сокет> -O check <хост>`: жив ли мастер.
    pub fn check_argv(&self) -> Vec<String> {
        self.control_argv("check", &[])
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

    /// Отпускает мастер-соединение этой сессии (вызывается при закрытии
    /// вкладки). Если сокетом больше никто в процессе не пользуется, мастер
    /// просят завершиться, не дожидаясь результата; ошибки не важны —
    /// мастера может уже не быть. Повторные вызовы (в том числе из клонов)
    /// ничего не делают.
    pub fn close_master(&self) {
        self.lease.release();
    }

    /// Место терминала в очереди входа через мастер этой сессии (см.
    /// [`LoginTicket`]).
    pub fn login_ticket(&self) -> LoginTicket {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        LoginTicket {
            control_path: self.control_path.clone(),
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        }
    }
}

// ------------------------------------------------------- очередь входа

/// Что делать терминалу, которому нужен ssh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginTurn {
    /// Мастер уже работает: ssh подключится через него без входа.
    MasterUp,
    /// Входить самому: никто другой сейчас не входит (или чужой вход не
    /// удался). Терминал считается входящим, пока не уйдёт из очереди.
    LogIn,
    /// Ждать: на этот сервер уже входят в другом терминале.
    Wait,
}

/// Идущий вход на один мастер: кто входит и кто ждёт за ним.
#[derive(Debug)]
struct PendingLogin {
    owner: u64,
    waiters: VecDeque<u64>,
}

/// Очереди входа процесса по сокетам мастеров. Только состояние, без
/// проверки самого мастера — её делает [`LoginTicket::turn`].
#[derive(Debug, Default)]
struct Logins {
    pending: HashMap<PathBuf, PendingLogin>,
}

impl Logins {
    /// Очередь терминала `id`, если мастер ещё не работает. Если входа
    /// нет, терминал становится входящим; если входит кто-то другой —
    /// встаёт в конец очереди ожидающих. Повторный вызов ничего не меняет.
    fn turn(&mut self, key: &Path, id: u64) -> LoginTurn {
        let Some(login) = self.pending.get_mut(key) else {
            let login = PendingLogin {
                owner: id,
                waiters: VecDeque::new(),
            };
            self.pending.insert(key.to_path_buf(), login);
            return LoginTurn::LogIn;
        };
        if login.owner == id {
            return LoginTurn::LogIn;
        }
        if !login.waiters.contains(&id) {
            login.waiters.push_back(id);
        }
        LoginTurn::Wait
    }

    /// Мастер поднялся: вход завершён, ожидающие свободны.
    fn master_up(&mut self, key: &Path) {
        self.pending.remove(key);
    }

    /// Терминал уходит из очереди: его ssh завершился, терминал закрыт или
    /// пользователь решил войти сам. Если уходит входящий, входить
    /// начинает первый из ожидающих, остальные ждут уже его.
    fn leave(&mut self, key: &Path, id: u64) {
        let Some(login) = self.pending.get_mut(key) else {
            return;
        };
        if login.owner != id {
            login.waiters.retain(|&waiter| waiter != id);
            return;
        }
        match login.waiters.pop_front() {
            Some(next) => login.owner = next,
            None => {
                self.pending.remove(key);
            }
        }
    }
}

static LOGINS: LazyLock<Mutex<Logins>> = LazyLock::new(|| Mutex::new(Logins::default()));

/// Слушает ли кто-нибудь сокет: `ssh` создаёт его, только войдя на сервер.
fn master_listening(control_path: &Path) -> bool {
    UnixStream::connect(control_path).is_ok()
}

/// Место терминала в очереди входа на общий мастер.
///
/// Пока первый терминал ждёт пароля, сокета мастера ещё нет, и `ssh`
/// второго терминала тоже стал бы входить сам и спросил бы пароль ещё раз.
/// Поэтому перед запуском ssh терминал спрашивает [`LoginTicket::turn`]:
/// если на этот сервер уже входят в другом терминале процесса, он ждёт и
/// повторяет вопрос, пока мастер не поднимется. Если тот вход не удался или
/// его терминал закрыли ([`LoginTicket::leave`]), входить начинает первый
/// из ожидающих. С другими экземплярами приложения очередь не согласуется.
#[derive(Debug)]
pub struct LoginTicket {
    control_path: PathBuf,
    id: u64,
}

impl LoginTicket {
    /// Можно ли запускать ssh. Ответ [`LoginTurn::LogIn`] делает терминал
    /// входящим, [`LoginTurn::Wait`] ставит его в очередь; ожидающий
    /// терминал вызывает функцию снова, пока не получит другой ответ.
    pub fn turn(&self) -> LoginTurn {
        let mut logins = LOGINS.lock().unwrap_or_else(|e| e.into_inner());
        if master_listening(&self.control_path) {
            logins.master_up(&self.control_path);
            return LoginTurn::MasterUp;
        }
        logins.turn(&self.control_path, self.id)
    }

    /// Уходит из очереди (ssh терминала завершился или терминал закрыт).
    /// Без очереди ничего не делает.
    pub fn leave(&self) {
        let mut logins = LOGINS.lock().unwrap_or_else(|e| e.into_inner());
        logins.leave(&self.control_path, self.id);
    }
}

impl Drop for LoginTicket {
    fn drop(&mut self) {
        self.leave();
    }
}

#[cfg(test)]
mod live_tests;

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
        assert_eq!(owner_pid("1234-m0123abcdef456789.sock"), Some(1234));
        assert_eq!(owner_pid("1234-m.sock"), None);
        assert_eq!(owner_pid("1234-mxyz.sock"), None);
        assert_eq!(owner_pid("files-1234-mabc"), None);
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
            s.control_argv("exit", &[]),
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
    fn forward_commands_go_through_the_master() {
        let s = session(Some("/tmp/alt_config"));
        let argv = s.control_argv("forward", &["-L", "8080:localhost:80"]);
        let sock = format!("/nonexistent-run/sshdeck/{}-7.sock", std::process::id());
        assert_eq!(
            argv,
            [
                SSH_BIN,
                "-F",
                "/tmp/alt_config",
                "-S",
                &sock,
                "-L",
                "8080:localhost:80",
                "-O",
                "forward",
                "--",
                "user@example",
            ]
        );
    }

    #[test]
    fn mux_commands_ignore_the_ssh_config() {
        let s = session(Some("/tmp/alt_config"));
        let argv = s.mux_argv("cancel", &["-D", "1080"]);
        let sock = format!("/nonexistent-run/sshdeck/{}-7.sock", std::process::id());
        assert_eq!(
            argv,
            [SSH_BIN, "-F", "/dev/null", "-S", &sock, "-D", "1080", "-O", "cancel", "--", "user@example"]
        );
    }

    #[test]
    fn master_key_depends_on_destination_and_config() {
        let a = Resolved::parse("user deploy\nhostname 10.0.0.5\nport 22\nidentityfile ~/.ssh/a\n");
        let same = Resolved::parse("hostname 10.0.0.5\nuser deploy\nport 22\nidentityfile ~/.ssh/b\n");
        let other_port = Resolved::parse("user deploy\nhostname 10.0.0.5\nport 2222\n");
        let other_user = Resolved::parse("user root\nhostname 10.0.0.5\nport 22\n");
        let key = master_key(Some(&a), "web", None);
        assert_eq!(key.len(), 16);
        assert_eq!(key, master_key(Some(&same), "deploy@10.0.0.5", None));
        assert_ne!(key, master_key(Some(&other_port), "web", None));
        assert_ne!(key, master_key(Some(&other_user), "web", None));
        assert_ne!(key, master_key(Some(&a), "web", Some(Path::new("/tmp/cfg"))));
        // Без ssh -G ключом служит сам адрес.
        assert_eq!(master_key(None, "web", None), master_key(None, "web", None));
        assert_ne!(master_key(None, "web", None), master_key(None, "db", None));
    }

    #[test]
    fn last_session_releases_the_master() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let first = Session::new("h", dir.path(), 1, None);
        let second = Session::new("h", dir.path(), 1, None);
        let other = Session::new("h", dir.path(), 2, None);
        assert_eq!(first.control_path(), second.control_path());
        assert_eq!(master_users(first.control_path()), 2);
        assert_eq!(master_users(other.control_path()), 1);

        // Клон — та же вкладка: повторное закрытие ничего не меняет.
        let clone = first.clone();
        first.close_master();
        clone.close_master();
        assert_eq!(master_users(second.control_path()), 1);
        drop(first);
        drop(clone);
        assert_eq!(master_users(second.control_path()), 1);

        // Последний клон, исчезнув, отпускает сокет сам.
        drop(second);
        assert_eq!(master_users(&dir.path().join(format!("{}-1.sock", std::process::id()))), 0);
        other.close_master();
        assert_eq!(master_users(other.control_path()), 0);
    }

    #[test]
    fn stale_socket_file_is_removed_by_the_first_user_only() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join(format!("{}-3.sock", std::process::id()));
        fs::write(&path, "").expect("write");
        let first = Session::new("h", dir.path(), 3, None);
        assert!(!path.exists(), "мёртвый сокет мешал бы ssh стать мастером");

        // Живой сокет первый пользователь не трогает.
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        first.close_master();
        let again = Session::new("h", dir.path(), 3, None);
        assert!(path.exists());
        drop(listener);
        drop(again);
    }

    #[test]
    fn tabs_to_the_same_destination_share_a_socket() {
        if !Path::new(SSH_BIN).exists() {
            return;
        }
        let dir = tempfile::TempDir::new().expect("tempdir");
        let config = dir.path().join("config");
        fs::write(
            &config,
            "Host web\n  HostName 192.0.2.10\n  User deploy\n\
             Host web-alias\n  HostName 192.0.2.10\n  User deploy\n  Port 22\n\
             Host web-root\n  HostName 192.0.2.10\n  User root\n",
        )
        .expect("write config");
        let run = dir.path().join("run");
        fs::create_dir(&run).expect("mkdir");
        let shared = |target: &str| Session::shared(target, &run, Some(config.clone()));
        let web = shared("web");
        let alias = shared("web-alias");
        let direct = shared("deploy@192.0.2.10");
        let root = shared("web-root");
        assert_eq!(web.control_path(), alias.control_path());
        assert_eq!(web.control_path(), direct.control_path());
        assert_ne!(web.control_path(), root.control_path());
        assert_eq!(master_users(web.control_path()), 3);
        let name = web.control_path().file_name().expect("name").to_string_lossy().into_owned();
        assert_eq!(owner_pid(&name), Some(std::process::id()));
        // Сокет не создаётся заранее: его откроет первый ssh.
        assert!(!web.control_path().exists());
    }

    #[test]
    fn control_commands() {
        let s = session(None);
        let check = s.check_argv();
        assert_eq!(&check[check.len() - 4..], ["-O", "check", "--", "user@example"]);
        let exit = s.control_argv("exit", &[]);
        assert_eq!(&exit[exit.len() - 4..], ["-O", "exit", "--", "user@example"]);
    }

    #[test]
    fn second_terminal_waits_for_the_login_in_progress() {
        let mut logins = Logins::default();
        let (key, other) = (Path::new("/run/a.sock"), Path::new("/run/b.sock"));
        assert_eq!(logins.turn(key, 1), LoginTurn::LogIn);
        assert_eq!(logins.turn(key, 2), LoginTurn::Wait);
        assert_eq!(logins.turn(key, 3), LoginTurn::Wait);
        // Повторные вопросы очередь не меняют.
        assert_eq!(logins.turn(key, 1), LoginTurn::LogIn);
        assert_eq!(logins.turn(key, 2), LoginTurn::Wait);
        assert_eq!(logins.pending[key].waiters, [2, 3]);
        // Другой сервер — своя очередь.
        assert_eq!(logins.turn(other, 4), LoginTurn::LogIn);

        // Мастер поднялся: очереди больше нет, следующий вход — с нуля.
        logins.master_up(key);
        assert!(!logins.pending.contains_key(key));
        logins.leave(key, 1);
        logins.leave(key, 2);
        assert_eq!(logins.turn(key, 5), LoginTurn::LogIn);
        assert!(logins.pending.contains_key(other));
    }

    #[test]
    fn exactly_one_waiter_takes_over_a_failed_login() {
        let mut logins = Logins::default();
        let key = Path::new("/run/a.sock");
        logins.turn(key, 1);
        logins.turn(key, 2);
        logins.turn(key, 3);
        logins.turn(key, 4);

        // Вход не удался: входит первый ожидающий, остальные ждут его.
        logins.leave(key, 1);
        assert_eq!(logins.turn(key, 2), LoginTurn::LogIn);
        assert_eq!(logins.turn(key, 3), LoginTurn::Wait);
        assert_eq!(logins.turn(key, 4), LoginTurn::Wait);
        // Бывший входящий, вернувшись (переподключение), встаёт в конец.
        assert_eq!(logins.turn(key, 1), LoginTurn::Wait);

        // Ожидающий закрыт: очередь его пропускает.
        logins.leave(key, 3);
        logins.leave(key, 2);
        assert_eq!(logins.turn(key, 4), LoginTurn::LogIn);
        assert_eq!(logins.turn(key, 1), LoginTurn::Wait);
        logins.leave(key, 4);
        assert_eq!(logins.turn(key, 1), LoginTurn::LogIn);
        logins.leave(key, 1);
        assert!(logins.pending.is_empty());
        // Уход без очереди ничего не делает.
        logins.leave(key, 9);
        assert!(logins.pending.is_empty());
    }

    #[test]
    fn tickets_see_the_master_socket() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let first = Session::new("h", dir.path(), 11, None);
        let second = Session::new("h", dir.path(), 11, None);
        let a = first.login_ticket();
        let b = second.login_ticket();
        let c = second.login_ticket();
        assert_eq!(a.turn(), LoginTurn::LogIn);
        assert_eq!(b.turn(), LoginTurn::Wait);
        assert_eq!(c.turn(), LoginTurn::Wait);

        // Терминал входящего закрыт до входа: входит следующий.
        drop(a);
        assert_eq!(b.turn(), LoginTurn::LogIn);
        assert_eq!(c.turn(), LoginTurn::Wait);

        // Мастер поднялся (сокет слушают): ждать больше некого.
        let listener =
            std::os::unix::net::UnixListener::bind(first.control_path()).expect("bind");
        assert_eq!(c.turn(), LoginTurn::MasterUp);
        assert_eq!(b.turn(), LoginTurn::MasterUp);
        drop(listener);
        let _ = fs::remove_file(first.control_path());

        // Мастер исчез (разрыв): вход снова идёт по очереди.
        assert_eq!(c.turn(), LoginTurn::LogIn);
        assert_eq!(b.turn(), LoginTurn::Wait);
        drop(c);
        assert_eq!(b.turn(), LoginTurn::LogIn);
        b.leave();
        // Терминал без входа (как после выхода из шелла) не мешает другим.
        assert_eq!(second.login_ticket().turn(), LoginTurn::LogIn);
    }
}
