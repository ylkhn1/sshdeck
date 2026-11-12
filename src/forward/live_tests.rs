//! Проброс портов через мастер на настоящем SSH-сервере. По умолчанию
//! пропускаются; запуск:
//!
//! ```sh
//! docker run -d --name sshdeck-test -p 127.0.0.1:2244:22 sshdeck-demo-server
//! SSHDECK_SSH_CONFIG=<конфиг> SSHDECK_TEST_SSH=<псевдоним> SSHDECK_TEST_SSH2=<второй псевдоним> \
//!     cargo test -- --ignored live_fwd_ --test-threads=1
//! ```
//!
//! Оба псевдонима ведут на один и тот же сервер и пускают по ключу без
//! пароля; на сервере нужны `nc` (busybox подходит) и `timeout`. Пробросы
//! ведут на sshd самого сервера (`localhost:22`): через работающий проброс
//! приходит приветствие `SSH-2.0-…`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Child;
use std::time::{Duration, Instant};

use super::*;
use crate::session::{config_override, runtime_dir};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("нужна переменная {name}"))
}

/// «Терминал» вкладки: ssh, который поднимает мастер и держит его.
fn start_master(session: &Session) -> Child {
    let mut argv = session.terminal_argv();
    argv.push("sleep 60".to_string());
    let child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("ssh");
    let deadline = Instant::now() + Duration::from_secs(15);
    while master_pid(session).is_none() {
        assert!(Instant::now() < deadline, "мастер не поднялся");
        std::thread::sleep(Duration::from_millis(100));
    }
    child
}

fn stop(session: &Session, mut child: Child) {
    session.close_master();
    let _ = child.kill();
    let _ = child.wait();
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn local(port: u16, dest_port: u16) -> Forward {
    Forward::from_fields(Kind::Local, "", &port.to_string(), "localhost", &dest_port.to_string())
        .expect("forward")
}

/// Первые байты, которые пришли по соединению.
fn read_banner(stream: &mut TcpStream) -> String {
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).expect("read");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// Через проброс на `port` отвечает sshd сервера.
fn assert_ssh_banner(port: u16) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let banner = read_banner(&mut stream);
    assert!(banner.starts_with("SSH-2.0-"), "{banner:?}");
}

/// Порт на этой машине больше никто не слушает.
fn assert_closed(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(("127.0.0.1", port)).is_ok() {
        assert!(Instant::now() < deadline, "порт {port} всё ещё открыт");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Локальный проброс работает, повтор безвреден, отмена закрывает порт;
/// занятый порт объясняется.
#[test]
#[ignore = "нужен SSHDECK_TEST_SSH"]
fn live_fwd_local_forward_and_cancel() {
    let session = Session::shared(&env("SSHDECK_TEST_SSH"), &runtime_dir().expect("run"), config_override());
    let master = start_master(&session);
    let port = free_port();
    let forward = local(port, 22);

    acquire(session.control_path(), &forward);
    apply(&session, &forward).expect("forward");
    assert_ssh_banner(port);
    // Тот же проброс просит вторая вкладка: мастер принимает повтор.
    acquire(session.control_path(), &forward);
    apply(&session, &forward).expect("повтор того же проброса");
    assert_ssh_banner(port);
    // Но порт, который держит мастер, для новой просьбы занят.
    assert!(!release(session.control_path(), &forward));
    assert!(release(session.control_path(), &forward));
    let err = apply(&session, &forward).expect_err("порт держит мастер");
    assert!(err.contains("already in use"), "{err}");

    cancel(&session, &forward).expect("cancel");
    assert_closed(port);
    let again = cancel(&session, &forward).expect_err("уже снят");
    assert!(again.contains("not forwarded"), "{again}");

    // Порт занят другой программой.
    let busy = TcpListener::bind("127.0.0.1:0").expect("bind");
    let busy_port = busy.local_addr().expect("addr").port();
    let err = apply(&session, &local(busy_port, 22)).expect_err("порт занят");
    eprintln!("{err}");
    assert!(err.contains("already in use"), "{err}");
    assert!(err.contains(&busy_port.to_string()), "{err}");
    drop(busy);

    stop(&session, master);
}

/// SOCKS-прокси: соединение с `localhost:22` «изнутри» сервера.
#[test]
#[ignore = "нужен SSHDECK_TEST_SSH"]
fn live_fwd_dynamic_socks() {
    let session = Session::shared(&env("SSHDECK_TEST_SSH"), &runtime_dir().expect("run"), config_override());
    let master = start_master(&session);
    let port = free_port();
    let forward = Forward::parse(&format!("-D {port}")).expect("forward");
    apply(&session, &forward).expect("forward");

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    // SOCKS5 без аутентификации.
    stream.write_all(&[5, 1, 0]).expect("hello");
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).expect("method");
    assert_eq!(reply, [5, 0]);
    let host = b"localhost";
    let mut request = vec![5, 1, 0, 3, host.len() as u8];
    request.extend_from_slice(host);
    request.extend_from_slice(&22u16.to_be_bytes());
    stream.write_all(&request).expect("connect request");
    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).expect("connect reply");
    assert_eq!(reply[1], 0, "SOCKS отказал: {reply:?}");
    let banner = read_banner(&mut stream);
    assert!(banner.starts_with("SSH-2.0-"), "{banner:?}");
    drop(stream);

    cancel(&session, &forward).expect("cancel");
    assert_closed(port);
    stop(&session, master);
}

/// Удалённый проброс: сервер подключается к порту на этой машине;
/// привилегированный порт на сервере — понятный отказ.
#[test]
#[ignore = "нужен SSHDECK_TEST_SSH (nc и timeout на сервере)"]
fn live_fwd_remote_forward() {
    let session = Session::shared(&env("SSHDECK_TEST_SSH"), &runtime_dir().expect("run"), config_override());
    let master = start_master(&session);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let local_port = listener.local_addr().expect("addr").port();
    let remote_port = 20000 + (std::process::id() % 20000) as u16;
    let forward = Forward::from_fields(
        Kind::Remote,
        "",
        &remote_port.to_string(),
        "127.0.0.1",
        &local_port.to_string(),
    )
    .expect("forward");
    apply(&session, &forward).expect("forward");

    let accept = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let banner = read_banner(&mut stream);
        stream.write_all(b"pong\n").expect("write");
        banner
    });
    let out = crate::remote::run(
        &session,
        &format!("echo ping-from-server | timeout 5 nc 127.0.0.1 {remote_port}"),
        "",
    )
    .expect("run");
    assert_eq!(accept.join().expect("thread").trim(), "ping-from-server");
    assert_eq!(out.stdout.trim(), "pong");

    cancel(&session, &forward).expect("cancel");
    let privileged = Forward::parse(&format!("-R 22:127.0.0.1:{local_port}")).expect("forward");
    let err = apply(&session, &privileged).expect_err("порт 22 на сервере занят");
    eprintln!("{err}");
    assert!(err.contains("remote port forwarding failed"), "{err}");
    stop(&session, master);
}

/// Две вкладки к одному серверу просят один и тот же проброс: первая его
/// убирает — он остаётся; снимается, только когда его отпускает вторая.
#[test]
#[ignore = "нужны SSHDECK_TEST_SSH и SSHDECK_TEST_SSH2"]
fn live_fwd_shared_forward_is_cancelled_by_the_last_tab() {
    let runtime = runtime_dir().expect("run");
    let first = Session::shared(&env("SSHDECK_TEST_SSH"), &runtime, config_override());
    let second = Session::shared(&env("SSHDECK_TEST_SSH2"), &runtime, config_override());
    assert_eq!(first.control_path(), second.control_path());
    let master = start_master(&first);
    let pid = master_pid(&first);

    let port = free_port();
    let forward = local(port, 22);
    let mut tab1 = Tracker::new(first.control_path());
    let mut tab2 = Tracker::new(second.control_path());
    for (tracker, session) in [(&mut tab1, &first), (&mut tab2, &second)] {
        tracker.add(forward.clone(), false).expect("add");
        for (job, generation) in tracker.master(pid) {
            let result = apply(session, &job);
            assert!(tracker.finish(&job, generation, result));
        }
        assert_eq!(tracker.active().count(), 1);
    }
    assert_eq!(users(first.control_path(), &forward), 2);
    assert_ssh_banner(port);

    // Первая вкладка убирает проброс: снимать его рано.
    assert_eq!(tab1.remove(&forward), Some(false));
    assert_ssh_banner(port);
    // Вторая закрывается — она последняя.
    assert_eq!(tab2.release_all(), vec![forward.clone()]);
    cancel(&second, &forward).expect("cancel");
    assert_closed(port);

    second.close_master();
    stop(&first, master);
}

/// Мастер пропал и поднялся заново (новый pid) — пробросы вкладки ставятся
/// снова.
#[test]
#[ignore = "нужен SSHDECK_TEST_SSH"]
fn live_fwd_forwards_come_back_after_reconnect() {
    let session = Session::shared(&env("SSHDECK_TEST_SSH"), &runtime_dir().expect("run"), config_override());
    let master = start_master(&session);
    let port = free_port();
    let forward = local(port, 22);
    let mut tracker = Tracker::new(session.control_path());
    tracker.add(forward.clone(), false).expect("add");
    let first_pid = master_pid(&session);
    for (job, generation) in tracker.master(first_pid) {
        let result = apply(&session, &job);
        assert!(tracker.finish(&job, generation, result));
    }
    assert_ssh_banner(port);

    // Разрыв: мастер завершается вместе с пробросами.
    let exit = session.control_argv("exit", &[]);
    assert!(Command::new(&exit[0]).args(&exit[1..]).status().expect("exit").success());
    let mut master = master;
    let _ = master.wait();
    assert_closed(port);
    assert!(tracker.master(master_pid(&session)).is_empty());
    assert_eq!(tracker.entries()[0].state, State::Waiting);

    // Переподключение.
    let master = start_master(&session);
    let second_pid = master_pid(&session);
    assert_ne!(first_pid, second_pid);
    let jobs = tracker.master(second_pid);
    assert_eq!(jobs.len(), 1);
    for (job, generation) in jobs {
        let result = apply(&session, &job);
        assert!(tracker.finish(&job, generation, result));
    }
    assert_ssh_banner(port);
    for forward in tracker.release_all() {
        cancel(&session, &forward).expect("cancel");
    }
    assert_closed(port);
    stop(&session, master);
}

/// `-O cancel` своего проброса не снимает `LocalForward` из конфига хоста.
#[test]
#[ignore = "нужны SSHDECK_TEST_SSH и SSHDECK_SSH_CONFIG"]
fn live_fwd_cancel_keeps_config_forwards() {
    let alias = env("SSHDECK_TEST_SSH");
    let base = env("SSHDECK_SSH_CONFIG");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config_port = free_port();
    let config = dir.path().join("config");
    std::fs::write(
        &config,
        format!("Host {alias}\n  LocalForward {config_port} localhost:22\n\nInclude {base}\n"),
    )
    .expect("config");
    let session = Session::shared(&alias, &runtime_dir().expect("run"), Some(config));
    let master = start_master(&session);
    assert_ssh_banner(config_port);

    let port = free_port();
    let forward = local(port, 22);
    apply(&session, &forward).expect("forward");
    assert_ssh_banner(port);
    cancel(&session, &forward).expect("cancel");
    assert_closed(port);
    // Проброс из конфига на месте.
    assert_ssh_banner(config_port);
    stop(&session, master);
}
