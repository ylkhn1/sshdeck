//! Проверки общего мастера, установки ключа и слежения за каталогом на
//! настоящем SSH-сервере. По умолчанию пропускаются; запуск:
//!
//! ```sh
//! docker run -d --name sshdeck-test -p 127.0.0.1:2242:22 sshdeck-demo-server
//! SSHDECK_SSH_CONFIG=<конфиг> SSHDECK_TEST_SSH=<псевдоним> SSHDECK_TEST_SSH2=<второй псевдоним> \
//!     cargo test -- --ignored live_ssh_ --test-threads=1
//! ```
//!
//! Оба псевдонима должны вести на один и тот же сервер (пользователь, хост,
//! порт) и пускать по ключу без пароля. Для установки ключа дополнительно:
//! `SSHDECK_TEST_KEY_ALIAS` — псевдоним того же сервера, в котором
//! `IdentityFile` указывает на ещё не существующий ключ внутри
//! `SSHDECK_TEST_KEY_HOME` (поддельный домашний каталог) и задан
//! `IdentitiesOnly yes`; `SSHDECK_TEST_MASTER_KEY` — ключ, с которым
//! входит сам «терминал»; `SSHDECK_TEST_EXPECT_LOGIN` — `works` или `fails`
//! (сервер читает ключи из другого файла).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::*;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("нужна переменная {name}"))
}

/// Запускает `ssh` так же, как терминал вкладки, но с удалённой командой
/// вместо интерактивного шелла. `extra` — опции перед адресом.
fn spawn_like_terminal(session: &Session, extra: &[&str], command: &str) -> Child {
    let mut argv = session.terminal_argv();
    let target = argv.pop().expect("target");
    argv.pop(); // "--"
    argv.extend(["-o".to_string(), "LogLevel=VERBOSE".to_string()]);
    argv.extend(extra.iter().map(|arg| arg.to_string()));
    argv.extend(["--".to_string(), target, command.to_string()]);
    Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ssh")
}

fn wait_for_master(session: &Session) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !crate::remote::master_alive(session) {
        assert!(Instant::now() < deadline, "мастер не поднялся");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn stderr_of(child: Child) -> (bool, String) {
    let output = child.wait_with_output().expect("wait");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Вторая «вкладка» к тому же серверу входит через мастер первой; выход
/// из шелла первой вкладки мастер не гасит; мастер завершается, когда его
/// отпускает последняя вкладка.
#[test]
#[ignore = "нужны SSHDECK_TEST_SSH и SSHDECK_TEST_SSH2"]
fn live_ssh_second_tab_reuses_the_master() {
    let runtime = runtime_dir().expect("runtime dir");
    let first = Session::shared(&env("SSHDECK_TEST_SSH"), &runtime, config_override());
    let second = Session::shared(&env("SSHDECK_TEST_SSH2"), &runtime, config_override());
    assert_eq!(first.control_path(), second.control_path());
    assert_eq!(master_users(first.control_path()), 2);

    // Первая вкладка входит на сервер и становится мастером.
    let tab1 = spawn_like_terminal(&first, &[], "sleep 4");
    wait_for_master(&first);
    // Вторая — клиент того же мастера, без собственной аутентификации.
    let tab2 = spawn_like_terminal(&second, &[], "sleep 10; echo tab2-alive");

    let (ok1, log1) = stderr_of(tab1);
    assert!(ok1, "{log1}");
    assert!(log1.contains("Authenticated to"), "первая вкладка должна войти: {log1}");
    // «Шелл» первой вкладки завершился, вкладка закрывается.
    first.close_master();
    assert_eq!(master_users(second.control_path()), 1);
    assert!(
        crate::remote::master_alive(&second),
        "мастер должен пережить первую вкладку"
    );
    let out = crate::remote::run(&second, "echo через-мастер", "").expect("run");
    assert_eq!(out.stdout.trim(), "через-мастер");

    let output = tab2.wait_with_output().expect("wait");
    let log2 = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{log2}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "tab2-alive");
    assert!(
        !log2.contains("Authenticated to"),
        "вторая вкладка не должна входить заново: {log2}"
    );

    // Последняя вкладка закрыта — мастер гасится.
    second.close_master();
    let deadline = Instant::now() + Duration::from_secs(5);
    while first.control_path().exists() {
        assert!(Instant::now() < deadline, "мастер не завершился");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Если мастер исчез (разрыв связи), «переподключение» любой вкладки
/// поднимает новый, и остальные снова ходят через него.
#[test]
#[ignore = "нужны SSHDECK_TEST_SSH и SSHDECK_TEST_SSH2"]
fn live_ssh_tabs_recover_after_master_loss() {
    let runtime = runtime_dir().expect("runtime dir");
    let first = Session::shared(&env("SSHDECK_TEST_SSH"), &runtime, config_override());
    let second = Session::shared(&env("SSHDECK_TEST_SSH2"), &runtime, config_override());
    let tab1 = spawn_like_terminal(&first, &[], "sleep 30");
    wait_for_master(&first);
    let tab2 = spawn_like_terminal(&second, &[], "sleep 30");
    std::thread::sleep(Duration::from_millis(500));

    // Мастер убит снаружи: обе вкладки теряют соединение (код 255).
    let exit = first.control_argv("exit", &[]);
    let status = Command::new(&exit[0]).args(&exit[1..]).status().expect("exit");
    assert!(status.success());
    for tab in [tab1, tab2] {
        let output = tab.wait_with_output().expect("wait");
        assert_eq!(output.status.code(), Some(255));
    }

    // «Переподключение» второй вкладки поднимает мастер заново.
    let tab2 = spawn_like_terminal(&second, &[], "sleep 3");
    wait_for_master(&second);
    let tab1 = spawn_like_terminal(&first, &[], "true");
    let (ok, log) = stderr_of(tab1);
    assert!(ok && !log.contains("Authenticated to"), "{log}");
    let (ok, _) = stderr_of(tab2);
    assert!(ok);
    first.close_master();
    second.close_master();
}

/// Вход без пароля с ключом из `IdentityFile` хоста: ключ создаётся по
/// этому пути в поддельном домашнем каталоге, ставится через мастер, а
/// пробный вход показывает, принял ли его сервер.
#[test]
#[ignore = "нужны SSHDECK_TEST_KEY_ALIAS, SSHDECK_TEST_KEY_HOME, SSHDECK_TEST_MASTER_KEY"]
fn live_ssh_key_setup_uses_host_identity_file() {
    use crate::keysetup::{self, Installed, LoginCheck};

    let alias = env("SSHDECK_TEST_KEY_ALIAS");
    let home = PathBuf::from(env("SSHDECK_TEST_KEY_HOME"));
    let master_key = env("SSHDECK_TEST_MASTER_KEY");
    let expect_works = std::env::var("SSHDECK_TEST_EXPECT_LOGIN").as_deref() != Ok("fails");
    let runtime = runtime_dir().expect("runtime dir");
    let session = Session::shared(&alias, &runtime, config_override());

    // «Терминал» входит своим ключом, указанным в командной строке.
    let identity = format!("IdentityFile={master_key}");
    let tab = spawn_like_terminal(&session, &["-o", &identity], "sleep 20");
    wait_for_master(&session);

    let plan = keysetup::plan(&alias, session.config(), &home).expect("plan");
    assert!(plan.private.starts_with(&home), "{plan:?}");
    assert!(plan.explicit, "{plan:?}");
    let created_before = plan.create;
    let outcome = keysetup::setup(&session, &plan).expect("setup");
    eprintln!("{outcome:?}");
    assert_eq!(outcome.created, created_before);
    assert!(Path::new(&plan.private).is_file());
    assert!(matches!(
        outcome.installed,
        Installed::Added | Installed::AlreadyPresent
    ));
    if expect_works {
        assert_eq!(outcome.login, LoginCheck::Works);
    } else {
        assert!(matches!(outcome.login, LoginCheck::Fails(_)), "{outcome:?}");
    }

    // Повторно: ключ уже есть и локально, и на сервере.
    let again = keysetup::plan(&alias, session.config(), &home).expect("plan");
    assert!(!again.create);
    assert_eq!(again.private, plan.private);
    let outcome = keysetup::setup(&session, &again).expect("setup");
    assert_eq!(outcome.installed, Installed::AlreadyPresent);

    session.close_master();
    let _ = stderr_of(tab);
}

/// Фрагмент слежения за каталогом на сервере: старая версия заменяется,
/// а bash сообщает каталог с пробелом и кириллицей так, что после
/// раскодирования получается тот же путь, а хост совпадает с `uname -n`.
#[test]
#[ignore = "нужен SSHDECK_TEST_SSH (bash на сервере)"]
fn live_ssh_cwd_snippet_on_server() {
    use crate::remote::{self, CwdTracking};

    let runtime = runtime_dir().expect("runtime dir");
    let session = Session::shared(&env("SSHDECK_TEST_SSH"), &runtime, config_override());
    let tab = spawn_like_terminal(&session, &[], "sleep 20");
    wait_for_master(&session);

    // На сервере стоит первая версия фрагмента.
    let v1 = "\n# sshdeck-cwd: report the current directory to the terminal (OSC 7)\n\
              __sshdeck_cwd() { printf '\\033]7;file://%s%s\\033\\\\' \"${HOSTNAME:-$HOST}\" \"$PWD\"; }\n\
              if [ -n \"$ZSH_VERSION\" ]; then precmd_functions+=(__sshdeck_cwd); else PROMPT_COMMAND=\"__sshdeck_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"; fi\n";
    remote::run(&session, &remote::sh("sed -i -e /sshdeck-cwd/d -e /__sshdeck_cwd/d ~/.bashrc; cat >> ~/.bashrc"), v1)
        .expect("seed v1");
    assert_eq!(remote::install_cwd_tracking(&session), Ok(CwdTracking::Updated));
    assert_eq!(remote::install_cwd_tracking(&session), Ok(CwdTracking::Present));
    let rc = remote::run(&session, "cat ~/.bashrc", "").expect("cat").stdout;
    assert_eq!(rc.matches("__sshdeck_cwd()").count(), 1, "{rc}");
    assert!(rc.contains("sshdeck-cwd v2"));

    let dir = "my dir/папка";
    let script = format!(
        "mkdir -p {q} && cd {q} && pwd && bash -ic {cmd} 2>/dev/null",
        q = remote::shell_quote(dir),
        cmd = remote::shell_quote("eval \"$PROMPT_COMMAND\""),
    );
    let out = remote::run(&session, &script, "").expect("run").stdout;
    let pwd = out.lines().next().expect("pwd").to_string();
    assert!(pwd.ends_with(dir), "{out}");
    let start = out.find("\x1b]7;").expect("OSC 7") + 4;
    let end = start + out[start..].find("\x1b\\").expect("ST");
    let uri = &out[start..end];
    eprintln!("OSC 7: {uri}");
    assert!(uri.is_ascii() && !uri.contains(' '), "{uri}");
    let reported = remote::parse_cwd_uri(uri).expect("uri");
    assert_eq!(String::from_utf8(reported.path).expect("utf8"), pwd);
    let host = remote::server_hostname(&session).expect("uname");
    assert!(remote::same_host(&host, &reported.host), "{host} / {}", reported.host);
    assert!(!remote::same_host(&host, "other-server"));

    session.close_master();
    let _ = stderr_of(tab);
}
