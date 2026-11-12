//! SSH-туннель до сервера базы данных.
//!
//! Порт базы пробрасывается не на локальный TCP-порт, а в Unix-сокет в
//! личном каталоге приложения (`$XDG_RUNTIME_DIR/sshdeck/db-<pid>-<n>`):
//! так не нужно искать свободный порт, и к туннелю не могут подключиться
//! другие пользователи машины. Расширениям нужен адрес и порт, поэтому для
//! них проброс идёт на свободный порт `127.0.0.1`.
//!
//! Если для этого сервера уже открыта вкладка, проброс добавляется в её
//! мастер-соединение (`ssh -O forward`) — пароль вводить не придётся.
//! Иначе запускается отдельный `ssh -N` без интерактивного входа: он
//! сработает, если вход настроен по ключу.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::session::{self, SSH_BIN};

/// Сколько ждать, пока поднимется туннель.
const OPEN_TIMEOUT: Duration = Duration::from_secs(25);
const POLL: Duration = Duration::from_millis(100);
const STDERR_LIMIT: u64 = 4096;

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// Куда пробрасывать.
pub struct Spec<'a> {
    /// SSH-сервер (адрес, уже проверенный `validate_target`).
    pub target: &'a str,
    /// Сокет мастер-соединения открытой вкладки к этому серверу.
    pub control: Option<&'a Path>,
    /// Сервер базы так, как его видит SSH-сервер.
    pub host: &'a str,
    pub port: u16,
    /// Куда пробрасывать на этой машине.
    pub local: Local<'a>,
}

/// Локальный конец туннеля.
pub enum Local<'a> {
    /// Сокет с этим именем в каталоге туннеля (PostgreSQL ищет
    /// `.s.PGSQL.<порт>`).
    Socket(&'a str),
    /// Свободный TCP-порт на 127.0.0.1.
    Tcp,
}

/// Работающий туннель; закрывается вместе со значением.
pub struct Tunnel {
    dir: PathBuf,
    socket: PathBuf,
    /// Порт на 127.0.0.1 для `Local::Tcp`.
    port: Option<u16>,
    mode: Mode,
}

enum Mode {
    /// Собственный `ssh -N`; завершается, когда `Child` уничтожается
    /// (`kill_on_drop`).
    Own { _child: Child },
    /// Проброс в чужом мастере: при закрытии его нужно отменить.
    Shared { cancel: Vec<String> },
}

impl Tunnel {
    /// Каталог с сокетом (для PostgreSQL это «хост»).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Сам сокет.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Локальный порт (для `Local::Tcp`).
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// Идёт ли туннель через соединение открытой вкладки.
    pub fn via_tab(&self) -> bool {
        matches!(self.mode, Mode::Shared { .. })
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Mode::Shared { cancel } = &self.mode {
            let spawned = std::process::Command::new(&cancel[0])
                .args(&cancel[1..])
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
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn base_argv() -> Vec<String> {
    let mut argv = vec![SSH_BIN.to_string()];
    if let Some(config) = session::config_override() {
        argv.push("-F".to_string());
        argv.push(config.to_string_lossy().into_owned());
    }
    argv
}

/// Адрес IPv6 для `ssh -L` берётся в скобки.
fn bracketed(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// `сокет:хост:порт` для `ssh -L`.
fn forward_spec(socket: &Path, host: &str, port: u16) -> String {
    format!("{}:{}:{port}", socket.display(), bracketed(host))
}

/// `127.0.0.1:локальный:хост:порт` для `ssh -L`.
fn tcp_forward_spec(local: u16, host: &str, port: u16) -> String {
    format!("127.0.0.1:{local}:{}:{port}", bracketed(host))
}

/// Свободный порт на 127.0.0.1 (его занимает ssh сразу после проверки).
fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .map_err(|err| tr!("Could not find a free port: {}", err))
}

/// Готов ли локальный конец туннеля.
async fn ready(socket: &Path, port: Option<u16>) -> bool {
    match port {
        Some(port) => tokio::time::timeout(
            Duration::from_secs(1),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .is_ok_and(|result| result.is_ok()),
        None => socket.exists(),
    }
}

fn control_argv(control: &Path, op: &str, forward: &str, target: &str) -> Vec<String> {
    let mut argv = base_argv();
    argv.extend([
        "-S".to_string(),
        control.to_string_lossy().replace('%', "%%"),
        "-O".to_string(),
        op.to_string(),
        "-L".to_string(),
        forward.to_string(),
        "--".to_string(),
        target.to_string(),
    ]);
    argv
}

fn own_argv(forward: &str, target: &str) -> Vec<String> {
    let mut argv = base_argv();
    for option in [
        "BatchMode=yes",
        "ExitOnForwardFailure=yes",
        "ControlMaster=no",
        "ControlPath=none",
        "RemoteCommand=none",
        "RequestTTY=no",
        "StreamLocalBindUnlink=yes",
        "ServerAliveInterval=30",
        "ConnectTimeout=15",
    ] {
        argv.push("-o".to_string());
        argv.push(option.to_string());
    }
    argv.extend([
        "-N".to_string(),
        "-L".to_string(),
        forward.to_string(),
        "--".to_string(),
        target.to_string(),
    ]);
    argv
}

fn command(argv: &[String]) -> Command {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Сообщение ssh без пустых строк; если его нет — `fallback`.
fn ssh_message(stderr: &[u8], fallback: &str) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        fallback.to_string()
    } else {
        lines.join("\n")
    }
}

async fn wait_for_socket(socket: &Path, port: Option<u16>) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if ready(socket, port).await {
            return true;
        }
        tokio::time::sleep(POLL).await;
    }
    ready(socket, port).await
}

/// Поднимает туннель и ждёт, пока сокет будет готов принимать соединения.
pub async fn open(spec: Spec<'_>) -> Result<Tunnel, String> {
    let runtime = session::runtime_dir()
        .map_err(|err| tr!("Could not prepare a directory for the tunnel: {}", err))?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let dir = runtime.join(format!("db-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    session::ensure_private_dir(&dir)
        .map_err(|err| tr!("Could not create {}: {}", dir.display(), err))?;
    let (socket, port, forward) = match spec.local {
        Local::Socket(name) => {
            let socket = dir.join(name);
            let forward = forward_spec(&socket, spec.host, spec.port);
            (socket, None, forward)
        }
        Local::Tcp => {
            let local = free_port()?;
            (
                dir.join("tcp"),
                Some(local),
                tcp_forward_spec(local, spec.host, spec.port),
            )
        }
    };

    // Через мастер открытой вкладки, если он жив.
    if let Some(control) = spec.control.filter(|path| path.exists()) {
        let argv = control_argv(control, "forward", &forward, spec.target);
        let cancel = control_argv(control, "cancel", &forward, spec.target);
        if let Ok(output) = command(&argv).output().await
            && output.status.success()
        {
            // Значение создаётся сразу: если сокет так и не появится,
            // проброс всё равно будет отменён.
            let tunnel = Tunnel {
                dir: dir.clone(),
                socket: socket.clone(),
                port,
                mode: Mode::Shared { cancel },
            };
            if wait_for_socket(&socket, port).await {
                return Ok(tunnel);
            }
            drop(tunnel);
            session::ensure_private_dir(&dir)
                .map_err(|err| tr!("Could not create {}: {}", dir.display(), err))?;
        }
        // Мастер не ответил — пробуем отдельное соединение.
    }

    let mut child = command(&own_argv(&forward, spec.target))
        .spawn()
        .map_err(|err| tr!("Could not start ssh: {}", err))?;
    let deadline = tokio::time::Instant::now() + OPEN_TIMEOUT;
    loop {
        if ready(&socket, port).await {
            return Ok(Tunnel {
                dir,
                socket,
                port,
                mode: Mode::Own { _child: child },
            });
        }
        if let Ok(Some(_status)) = child.try_wait() {
            let mut stderr = Vec::new();
            if let Some(pipe) = child.stderr.take() {
                let _ = pipe.take(STDERR_LIMIT).read_to_end(&mut stderr).await;
            }
            let _ = std::fs::remove_dir_all(&dir);
            let message = ssh_message(&stderr, tr!("ssh exited without giving a reason"));
            return Err(tr!(
                "The SSH tunnel to {} could not be opened:\n{}\n\nOpen a tab with this server \
                 (the tunnel will use its connection) or set up passwordless login with a key.",
                spec.target,
                message
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(tr!(
                "The SSH tunnel to {} did not open within {} s",
                spec.target,
                OPEN_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_specs() {
        let socket = Path::new("/run/user/1/sshdeck/db-1-1/.s.PGSQL.5432");
        assert_eq!(
            forward_spec(socket, "localhost", 5432),
            "/run/user/1/sshdeck/db-1-1/.s.PGSQL.5432:localhost:5432"
        );
        assert_eq!(
            forward_spec(socket, "::1", 5432),
            "/run/user/1/sshdeck/db-1-1/.s.PGSQL.5432:[::1]:5432"
        );
        assert_eq!(
            tcp_forward_spec(40000, "db", 8123),
            "127.0.0.1:40000:db:8123"
        );
        assert!(free_port().is_ok_and(|port| port > 0));
    }

    #[test]
    fn own_tunnel_never_prompts() {
        let argv = own_argv("/s:db:3306", "root@host");
        assert!(argv.contains(&"BatchMode=yes".to_string()));
        assert!(argv.contains(&"ExitOnForwardFailure=yes".to_string()));
        assert_eq!(
            &argv[argv.len() - 5..],
            ["-N", "-L", "/s:db:3306", "--", "root@host"]
        );
    }

    #[test]
    fn shared_tunnel_commands() {
        let argv = control_argv(Path::new("/run/a%b.sock"), "cancel", "/s:db:1", "h");
        assert!(argv.contains(&"/run/a%%b.sock".to_string()));
        assert_eq!(
            &argv[argv.len() - 6..],
            ["-O", "cancel", "-L", "/s:db:1", "--", "h"]
        );
    }

    #[test]
    fn ssh_messages() {
        assert_eq!(ssh_message(b"\n  \n", "x"), "x");
        assert_eq!(
            ssh_message(b"Permission denied (publickey).\r\n", "x"),
            "Permission denied (publickey)."
        );
    }
}
