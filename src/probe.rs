//! Проверка, отвечает ли сервер, без входа на него.
//!
//! Адрес раскрывается через `ssh -G` (тот же конфиг, что и при
//! подключении: псевдонимы, `HostName`, `Port`), затем открывается TCP-
//! соединение и читается приветствие `SSH-…`. Аутентификации нет, поэтому
//! в журнале сервера остаётся лишь строка о закрытом соединении, а не
//! неудачная попытка входа.
//!
//! Хосты за `ProxyJump`/`ProxyCommand` напрямую не проверяются: прямое
//! подключение к ним ничего не говорит о доступности через прокси.

use std::io::Read;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::session::{self, SSH_BIN};

/// Сколько ждать соединения и приветствия.
const TIMEOUT: Duration = Duration::from_secs(4);
/// До приветствия сервер вправе прислать несколько строк текста
/// (RFC 4253, 4.2); дальше этого объёма не читаем.
const MAX_BANNER_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Сервер ответил приветствием SSH; задержка установки TCP-соединения.
    Online { latency_ms: u32 },
    /// Не ответил; причина для подсказки.
    Offline(String),
    /// Проверить нельзя (прокси, ошибка в адресе).
    Unknown(String),
}

impl Status {
    /// Подсказка к значку состояния.
    pub fn describe(&self) -> String {
        match self {
            Self::Online { latency_ms } => tr!("Online · {} ms", latency_ms),
            Self::Offline(reason) => tr!("Unreachable: {}", reason),
            Self::Unknown(reason) => tr!("Not checked: {}", reason),
        }
    }
}

/// Куда на самом деле пойдёт ssh.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Direct { host: String, port: u16 },
    /// Через прокси; название параметра конфига.
    Proxied(&'static str),
}

/// Разбирает вывод `ssh -G`: строки `ключ значение`.
fn parse_resolved(text: &str) -> Option<Route> {
    let mut host = None;
    let mut port = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key {
            "hostname" => host = Some(value),
            "port" => port = value.parse().ok(),
            "proxyjump" if value != "none" => return Some(Route::Proxied("ProxyJump")),
            "proxycommand" if value != "none" => return Some(Route::Proxied("ProxyCommand")),
            _ => {}
        }
    }
    Some(Route::Direct {
        host: host.filter(|h| !h.is_empty())?.to_string(),
        port: port?,
    })
}

/// Проверяет адрес в том виде, в каком он хранится в списке (`user@host
/// -p 2222`, псевдоним и т. п.). Блокирует до [`TIMEOUT`] на каждый адрес
/// хоста — вызывать вне потока GTK.
pub fn check(address: &str, config: Option<&Path>) -> Status {
    let target = match session::validate_target(address) {
        Ok(target) => target,
        Err(err) => return Status::Unknown(err),
    };
    let mut command = Command::new(SSH_BIN);
    if let Some(config) = config {
        command.arg("-F").arg(config);
    }
    let output = command
        .args(["-G", "--", &target])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let route = match output {
        Ok(output) if output.status.success() => {
            parse_resolved(&String::from_utf8_lossy(&output.stdout))
        }
        Ok(_) => None,
        Err(err) => return Status::Unknown(tr!("could not start ssh: {}", err)),
    };
    match route {
        Some(Route::Direct { host, port }) => connect(&host, port),
        Some(Route::Proxied(kind)) => Status::Unknown(tr!("connects through {}", kind)),
        None => Status::Unknown(tr!("ssh could not parse the address").to_string()),
    }
}

fn connect(host: &str, port: u16) -> Status {
    let addresses: Vec<SocketAddr> = match (host, port).to_socket_addrs() {
        Ok(addresses) => addresses.collect(),
        Err(_) => return Status::Offline(tr!("name {} not found", host)),
    };
    let mut last_error = tr!("{} has no addresses", host);
    for address in addresses {
        let started = Instant::now();
        match TcpStream::connect_timeout(&address, TIMEOUT) {
            Ok(stream) => {
                let latency_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
                return match read_banner(stream) {
                    Ok(()) => Status::Online { latency_ms },
                    Err(reason) => Status::Offline(reason),
                };
            }
            Err(err) => last_error = describe_io(&err, port),
        }
    }
    Status::Offline(last_error)
}

/// Ждёт строку `SSH-…`. Своё приветствие не отправляем: соединение
/// закрывается до обмена ключами.
fn read_banner(mut stream: TcpStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(|err| err.to_string())?;
    let mut received = Vec::new();
    let mut chunk = [0u8; 512];
    while received.len() < MAX_BANNER_BYTES {
        let read = match stream.read(&mut chunk) {
            Ok(0) => return Err(tr!("the server closed the connection without an SSH greeting").to_string()),
            Ok(read) => read,
            Err(err) if is_timeout(&err) => {
                return Err(tr!("the port is open, but there is no SSH greeting").to_string());
            }
            Err(err) => return Err(describe_io(&err, 0)),
        };
        received.extend_from_slice(&chunk[..read]);
        if has_ssh_banner(&received) {
            return Ok(());
        }
    }
    Err(tr!("something other than SSH answers on the port").to_string())
}

/// Есть ли среди полностью принятых строк приветствие `SSH-`.
fn has_ssh_banner(received: &[u8]) -> bool {
    let complete = match received.iter().rposition(|&b| b == b'\n') {
        Some(end) => &received[..end],
        None => return false,
    };
    complete.split(|&b| b == b'\n').any(|line| line.starts_with(b"SSH-"))
}

fn is_timeout(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

fn describe_io(err: &std::io::Error, port: u16) -> String {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::ConnectionRefused => tr!("port {} is closed", port),
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => {
            tr!("the server reset the connection").to_string()
        }
        ErrorKind::TimedOut | ErrorKind::WouldBlock => tr!("no response").to_string(),
        ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable => {
            tr!("no route to the server").to_string()
        }
        _ => err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn resolves_direct_route() {
        let text = "user toomag\nhostname 212.116.233.116\nport 18122\nproxyjump none\n";
        assert_eq!(
            parse_resolved(text),
            Some(Route::Direct {
                host: "212.116.233.116".to_string(),
                port: 18122
            })
        );
    }

    #[test]
    fn proxied_hosts_are_not_probed() {
        let jump = "hostname inner\nport 22\nproxyjump bastion\n";
        assert_eq!(parse_resolved(jump), Some(Route::Proxied("ProxyJump")));
        let command = "hostname inner\nport 22\nproxycommand nc %h %p\n";
        assert_eq!(parse_resolved(command), Some(Route::Proxied("ProxyCommand")));
    }

    #[test]
    fn incomplete_output_is_rejected() {
        assert_eq!(parse_resolved("hostname x\n"), None);
        assert_eq!(parse_resolved("port 22\n"), None);
        assert_eq!(parse_resolved(""), None);
    }

    #[test]
    fn banner_detection() {
        assert!(has_ssh_banner(b"SSH-2.0-OpenSSH_9.6\r\n"));
        assert!(has_ssh_banner(b"Welcome\r\nSSH-2.0-x\r\n"));
        assert!(!has_ssh_banner(b"SSH-2.0-OpenSSH")); // строка ещё не дошла
        assert!(!has_ssh_banner(b"HTTP/1.1 400 Bad Request\r\n"));
    }

    fn serve_once(reply: &'static [u8]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(reply);
            }
        });
        port
    }

    #[test]
    fn ssh_server_is_online() {
        let port = serve_once(b"SSH-2.0-test\r\n");
        assert!(matches!(connect("127.0.0.1", port), Status::Online { .. }));
    }

    #[test]
    fn other_service_is_offline() {
        let port = serve_once(b"220 smtp ready\r\n");
        assert!(matches!(connect("127.0.0.1", port), Status::Offline(_)));
    }

    #[test]
    fn closed_port_is_offline() {
        // Освобождённый порт может тут же занять соседний тест, поэтому
        // проверяется только итог, а текст причины — отдельно.
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        assert!(matches!(connect("127.0.0.1", port), Status::Offline(_)));
    }

    #[test]
    fn errors_are_explained() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            describe_io(&Error::from(ErrorKind::ConnectionRefused), 2222),
            "port 2222 is closed"
        );
        assert_eq!(describe_io(&Error::from(ErrorKind::TimedOut), 22), "no response");
        assert_eq!(
            describe_io(&Error::from(ErrorKind::ConnectionReset), 22),
            "the server reset the connection"
        );
    }
}
