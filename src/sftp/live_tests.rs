//! Проверки SFTP-слоя на настоящем сервере. По умолчанию пропускаются;
//! нужен SSH-сервер, пускающий по ключу без пароля, с bash у пользователя:
//!
//! ```sh
//! SSHDECK_SSH_CONFIG=<конфиг> SSHDECK_TEST_SSH=<псевдоним> \
//!     cargo test sftp::live_ -- --ignored --test-threads=1
//! ```
//!
//! Каждая проверка работает в собственном каталоге `~/sshdeck-live-*` на
//! сервере и удаляет его в конце.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Stdio};
use std::time::Instant;

use super::*;
use crate::session::{self, Session};

const TIMEOUT: Duration = Duration::from_secs(60);

fn raw(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

/// Мастер-соединение «вкладки» и SFTP-задача поверх него, как в приложении.
struct Live {
    session: Session,
    master: Child,
    worker: Option<SftpWorker>,
    events: async_channel::Receiver<Event>,
    home: PathBuf,
    /// Рабочий каталог проверки на сервере.
    dir: PathBuf,
    next_id: std::cell::Cell<TransferId>,
}

impl Live {
    fn start(name: &str) -> Self {
        let alias = std::env::var("SSHDECK_TEST_SSH").expect("SSHDECK_TEST_SSH");
        let runtime = session::runtime_dir().expect("runtime dir");
        static NEXT_TAB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let tab = 7000 + (std::process::id() % 1000) * 10 + NEXT_TAB.fetch_add(1, Ordering::Relaxed);
        let session = Session::new(&alias, &runtime, tab, session::config_override());
        let mut argv = session.terminal_argv();
        let target = argv.pop().expect("target");
        argv.pop(); // "--"
        argv.extend(["-o".into(), "BatchMode=yes".into(), "-N".into(), "--".into(), target]);
        let master = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .spawn()
            .expect("ssh master");
        let deadline = Instant::now() + Duration::from_secs(15);
        while !session.control_path().exists() {
            assert!(Instant::now() < deadline, "мастер не поднялся");
            std::thread::sleep(Duration::from_millis(100));
        }

        let (tx, events) = async_channel::unbounded();
        let worker = SftpWorker::start(session.clone(), tx).expect("worker");
        let mut live = Self {
            session,
            master,
            worker: Some(worker),
            events,
            home: PathBuf::new(),
            dir: PathBuf::new(),
            next_id: std::cell::Cell::new(1),
        };
        live.home = live.wait(|event| match event {
            Event::State(ConnState::Connected { home }) => Some(home),
            _ => None,
        });
        let dir = join_remote(&live.home, format!("sshdeck-live-{name}"));
        live.shell(&format!("rm -rf '{0}' && mkdir '{0}'", display(&dir)));
        live.dir = dir;
        live
    }

    fn send(&self, command: Command) {
        self.worker.as_ref().expect("worker").send(command);
    }

    fn wait<T>(&self, mut pick: impl FnMut(Event) -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match self.events.try_recv() {
                Ok(event) => {
                    if let Some(value) = pick(event) {
                        return value;
                    }
                }
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "не дождались события");
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => panic!("канал закрыт: {err}"),
            }
        }
    }

    /// Команда bash в рабочем каталоге проверки; возвращает stdout.
    fn shell(&self, script: &str) -> Vec<u8> {
        let script = if self.dir.as_os_str().is_empty() {
            script.to_string()
        } else {
            format!("cd '{}' && {script}", display(&self.dir))
        };
        let argv = self.session.command_argv(&format!("bash -c {}", crate::remote::shell_quote(&script)));
        let output = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .output()
            .expect("ssh");
        assert!(
            output.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn shell_text(&self, script: &str) -> String {
        String::from_utf8_lossy(&self.shell(script)).trim().to_string()
    }

    fn list(&self, path: &Path) -> Vec<RemoteEntry> {
        self.send(Command::List {
            path: path.to_path_buf(),
        });
        self.wait(|event| match event {
            Event::Listing { path: done, result } if done == path => {
                Some(result.expect("listing"))
            }
            _ => None,
        })
    }

    fn op(&self, command: Command) -> Option<String> {
        self.send(command);
        self.wait(|event| match event {
            Event::OpDone { error, .. } => Some(error),
            _ => None,
        })
    }

    fn transfer(&self, make: impl FnOnce(TransferId) -> Command) -> TransferOutcome {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.send(make(id));
        self.wait(|event| match event {
            Event::TransferDone { id: done, outcome } if done == id => Some(outcome),
            _ => None,
        })
    }

    fn path(&self, name: impl AsRef<OsStr>) -> PathBuf {
        join_remote(&self.dir, name)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if !self.dir.as_os_str().is_empty() {
            let dir = std::mem::take(&mut self.dir);
            let argv = self
                .session
                .command_argv(&format!("rm -rf '{}'", display(&dir)));
            let _ = std::process::Command::new(&argv[0]).args(&argv[1..]).status();
        }
        self.worker.take();
        let _ = self.master.kill();
        let _ = self.master.wait();
        let _ = std::fs::remove_file(self.session.control_path());
    }
}

fn completed(outcome: TransferOutcome) -> (Vec<PathBuf>, Option<RemoteFile>) {
    match outcome {
        TransferOutcome::Completed {
            skipped,
            size_mismatch,
            file,
        } => {
            assert!(size_mismatch.is_empty(), "размер не сошёлся: {size_mismatch:?}");
            (skipped, file)
        }
        other => panic!("передача не удалась: {other:?}"),
    }
}

fn names(paths: &[PathBuf]) -> BTreeSet<String> {
    paths.iter().map(|p| display(Path::new(base_name(p)))).collect()
}

fn set_mtime(path: &Path, secs: u64) {
    let file = std::fs::File::open(path).expect("open");
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
        .expect("set mtime");
}

fn mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .expect("metadata")
        .modified()
        .expect("mtime")
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("epoch")
        .as_secs()
}

use std::time::SystemTime;

#[test]
#[ignore = "нужен SSHDECK_TEST_SSH"]
fn live_non_utf8_names() {
    let live = Live::start("names");
    live.shell(r"printf data > $'\xff\xfe.txt' && mkdir $'d\xe9' && printf x > $'d\xe9/in'");

    // Список: имена байт в байт, тип определён.
    let entries = live.list(&live.dir);
    let file = entries
        .iter()
        .find(|e| e.name == raw(b"\xff\xfe.txt"))
        .expect("файл с не-UTF-8 именем в списке");
    assert_eq!(file.kind, EntryKind::File);
    assert_eq!(file.size, Some(4));
    assert_eq!(file.display_name(), "\u{fffd}\u{fffd}.txt");
    let dir = entries.iter().find(|e| e.name == raw(b"d\xe9")).expect("каталог");
    assert_eq!(dir.kind, EntryKind::Dir);
    assert_eq!(live.list(&dir.path).len(), 1);

    // Переход в каталог с таким именем.
    live.send(Command::Navigate {
        path: dir.path.clone(),
    });
    let (path, inside) = live.wait(|event| match event {
        Event::Navigated { result, .. } => Some(result.expect("navigate")),
        _ => None,
    });
    assert_eq!(path, dir.path);
    assert_eq!(inside[0].name, "in");

    // Переименование.
    let renamed = live.path(raw(b"r\xff.txt"));
    assert_eq!(
        live.op(Command::Rename {
            from: file.path.clone(),
            to: renamed.clone(),
        }),
        None
    );
    assert_eq!(live.shell(r"ls"), b"d\xe9\nr\xff.txt\n");

    // Скачивание файла и каталога.
    let local = tempfile::tempdir().expect("tempdir");
    let (_, single) = completed(live.transfer(|id| Command::Download {
        id,
        sources: vec![renamed.clone()],
        dest_dir: local.path().to_path_buf(),
    }));
    assert_eq!(single.expect("итог одиночного файла").path, renamed);
    assert_eq!(std::fs::read(local.path().join(raw(b"r\xff.txt"))).expect("read"), b"data");
    completed(live.transfer(|id| Command::Download {
        id,
        sources: vec![dir.path.clone()],
        dest_dir: local.path().to_path_buf(),
    }));
    assert_eq!(std::fs::read(local.path().join(raw(b"d\xe9")).join("in")).expect("read"), b"x");

    // Загрузка локального файла с не-UTF-8 именем внутрь каталога.
    let upload = local.path().join(raw(b"up\xfa"));
    std::fs::write(&upload, b"up").expect("write");
    completed(live.transfer(|id| Command::Upload {
        id,
        sources: vec![upload.clone()],
        dest_dir: dir.path.clone(),
    }));
    assert_eq!(live.shell(r"cat $'d\xe9/up\xfa'"), b"up");

    // Сохранение из редактора по такому пути.
    std::fs::write(&upload, b"edited").expect("write");
    let target = join_remote(&dir.path, raw(b"up\xfa"));
    completed(live.transfer(|id| Command::UploadFile {
        id,
        local: upload.clone(),
        remote: target.clone(),
        mode: SaveMode::Overwrite,
    }));
    assert_eq!(live.shell(r"cat $'d\xe9/up\xfa'"), b"edited");

    // Удаление файла и каталога с содержимым.
    assert_eq!(
        live.op(Command::Delete {
            paths: vec![renamed, dir.path.clone()],
        }),
        None
    );
    assert_eq!(live.shell("ls -A | wc -l"), b"0\n");
}

#[test]
#[ignore = "нужен SSHDECK_TEST_SSH"]
fn live_times_and_permissions() {
    let live = Live::start("modes");
    let local = tempfile::tempdir().expect("tempdir");

    // Новый файл получает права и время исходного.
    let script = local.path().join("run.sh");
    std::fs::write(&script, b"#!/bin/sh\n").expect("write");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o4750)).expect("chmod");
    set_mtime(&script, 1_600_000_000);
    // Каталог с файлом внутри; сам каталог только для чтения.
    let tree = local.path().join("tree");
    std::fs::create_dir(&tree).expect("mkdir");
    std::fs::write(tree.join("a"), b"a").expect("write");
    set_mtime(&tree.join("a"), 1_500_000_000);
    std::fs::set_permissions(&tree, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    set_mtime(&tree, 1_400_000_000);
    completed(live.transfer(|id| Command::Upload {
        id,
        sources: vec![script.clone(), tree.clone()],
        dest_dir: live.dir.clone(),
    }));
    std::fs::set_permissions(&tree, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_eq!(live.shell_text("stat -c '%a %Y' run.sh"), "750 1600000000");
    assert_eq!(live.shell_text("stat -c '%a %Y' tree/a"), "644 1500000000");
    assert_eq!(live.shell_text("stat -c '%a %Y' tree"), "555 1400000000");
    live.shell("chmod 755 tree");

    // Существующий файл сохраняет свои права, но время берёт у исходного.
    live.shell("printf old > keep.txt && chmod 600 keep.txt");
    let keep = local.path().join("keep.txt");
    std::fs::write(&keep, b"new contents").expect("write");
    std::fs::set_permissions(&keep, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    set_mtime(&keep, 1_650_000_000);
    completed(live.transfer(|id| Command::Upload {
        id,
        sources: vec![keep.clone()],
        dest_dir: live.dir.clone(),
    }));
    assert_eq!(live.shell_text("stat -c '%a %Y' keep.txt; cat keep.txt"), "600 1650000000\nnew contents");

    // Скачивание переносит время изменения с сервера.
    live.shell("mkdir -p dl/sub && printf r > dl/sub/r && touch -d @1300000000 dl/sub/r && touch -d @1200000000 dl/sub dl");
    let out = tempfile::tempdir().expect("tempdir");
    completed(live.transfer(|id| Command::Download {
        id,
        sources: vec![live.path("dl")],
        dest_dir: out.path().to_path_buf(),
    }));
    assert_eq!(mtime(&out.path().join("dl/sub/r")), 1_300_000_000);
    assert_eq!(mtime(&out.path().join("dl/sub")), 1_200_000_000);
    assert_eq!(mtime(&out.path().join("dl")), 1_200_000_000);
}

#[test]
#[ignore = "нужен SSHDECK_TEST_SSH"]
fn live_large_file_round_trip() {
    let live = Live::start("large");
    let local = tempfile::tempdir().expect("tempdir");
    // Размер не кратен куску, чтобы проверить хвост.
    let mut data = vec![0u8; 5 * 1024 * 1024 + 12345];
    let mut seed = 0x1234_5678u32;
    for byte in &mut data {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        *byte = (seed >> 16) as u8;
    }
    let source = local.path().join("big.bin");
    std::fs::write(&source, &data).expect("write");
    completed(live.transfer(|id| Command::Upload {
        id,
        sources: vec![source.clone()],
        dest_dir: live.dir.clone(),
    }));
    let remote_sum = live.shell_text("sha256sum big.bin | cut -d' ' -f1");
    let local_sum = String::from_utf8_lossy(
        &std::process::Command::new("sha256sum")
            .arg(&source)
            .output()
            .expect("sha256sum")
            .stdout,
    )
    .split(' ')
    .next()
    .unwrap_or_default()
    .to_string();
    assert_eq!(remote_sum, local_sum);

    let back = tempfile::tempdir().expect("tempdir");
    completed(live.transfer(|id| Command::Download {
        id,
        sources: vec![live.path("big.bin")],
        dest_dir: back.path().to_path_buf(),
    }));
    assert!(std::fs::read(back.path().join("big.bin")).expect("read") == data);
}

#[test]
#[ignore = "нужен SSHDECK_TEST_SSH"]
fn live_follows_directory_links() {
    let live = Live::start("links");
    // real/sub/f, ссылка на real, ссылки по кругу, канал, битая ссылка.
    live.shell(
        "mkdir -p t/real/sub && printf f > t/real/sub/f && ln -s real t/link \
         && ln -s .. t/real/up && ln -s ../../real t/real/sub/again && mkfifo t/fifo \
         && ln -s nowhere t/broken",
    );
    let local = tempfile::tempdir().expect("tempdir");
    let (skipped, _) = completed(live.transfer(|id| Command::Download {
        id,
        sources: vec![live.path("t")],
        dest_dir: local.path().to_path_buf(),
    }));
    let t = local.path().join("t");
    assert_eq!(std::fs::read(t.join("real/sub/f")).expect("real"), b"f");
    assert_eq!(std::fs::read(t.join("link/sub/f")).expect("через ссылку"), b"f");
    assert!(std::fs::symlink_metadata(t.join("link")).expect("link").is_dir());
    assert!(!t.join("real/up").exists());
    assert!(!t.join("real/sub/again").exists());
    assert_eq!(
        names(&skipped),
        ["again", "broken", "fifo", "up"].iter().map(|s| s.to_string()).collect()
    );

    // В обратную сторону: локальное дерево со ссылками.
    let src = tempfile::tempdir().expect("tempdir");
    let root = src.path().join("u");
    std::fs::create_dir_all(root.join("real/sub")).expect("mkdir");
    std::fs::write(root.join("real/sub/f"), b"f").expect("write");
    std::os::unix::fs::symlink("real", root.join("link")).expect("symlink");
    std::os::unix::fs::symlink("..", root.join("real/up")).expect("symlink");
    std::os::unix::fs::symlink("../../real", root.join("real/sub/again")).expect("symlink");
    std::os::unix::fs::symlink("nowhere", root.join("broken")).expect("symlink");
    let (skipped, _) = completed(live.transfer(|id| Command::Upload {
        id,
        sources: vec![root.clone()],
        dest_dir: live.dir.clone(),
    }));
    assert_eq!(live.shell_text("cat u/real/sub/f u/link/sub/f"), "ff");
    assert_eq!(live.shell_text("test -L u/link || echo dir"), "dir");
    assert_eq!(live.shell_text("ls u/real; ls u/real/sub"), "sub\nf");
    assert_eq!(
        names(&skipped),
        ["again", "broken", "up"].iter().map(|s| s.to_string()).collect()
    );
}

#[test]
#[ignore = "нужен SSHDECK_TEST_SSH"]
fn live_editor_save_detects_changes() {
    let live = Live::start("edit");
    live.shell("printf v1 > notes.txt");
    let local = tempfile::tempdir().expect("tempdir");
    let remote = live.path("notes.txt");
    let (_, file) = completed(live.transfer(|id| Command::Download {
        id,
        sources: vec![remote.clone()],
        dest_dir: local.path().to_path_buf(),
    }));
    let opened = file.expect("состояние после скачивания").stamp;
    assert_eq!(opened.size, Some(2));
    let copy = local.path().join("notes.txt");

    // Никто не трогал — сохраняется, состояние обновляется.
    std::fs::write(&copy, b"v2 local").expect("write");
    let (_, file) = completed(live.transfer(|id| Command::UploadFile {
        id,
        local: copy.clone(),
        remote: remote.clone(),
        mode: SaveMode::IfUnchanged(opened),
    }));
    let saved = file.expect("состояние после сохранения");
    assert_eq!(saved.path, remote);
    assert_eq!(saved.stamp.size, Some(8));
    assert_eq!(live.shell("cat notes.txt"), b"v2 local");

    // Файл изменили на сервере — заливки нет, сообщается новое состояние.
    live.shell("printf 'changed on server' > notes.txt");
    std::fs::write(&copy, b"v3 local").expect("write");
    match live.transfer(|id| Command::UploadFile {
        id,
        local: copy.clone(),
        remote: remote.clone(),
        mode: SaveMode::IfUnchanged(saved.stamp),
    }) {
        TransferOutcome::Conflict { current } => {
            assert_eq!(current.expect("файл есть").size, Some(17));
        }
        other => panic!("ожидался конфликт: {other:?}"),
    }
    assert_eq!(live.shell("cat notes.txt"), b"changed on server");

    // Копия рядом, затем вторая копия.
    let (_, file) = completed(live.transfer(|id| Command::UploadFile {
        id,
        local: copy.clone(),
        remote: remote.clone(),
        mode: SaveMode::Copy,
    }));
    assert_eq!(file.expect("копия").path, live.path("notes (local copy).txt"));
    completed(live.transfer(|id| Command::UploadFile {
        id,
        local: copy.clone(),
        remote: remote.clone(),
        mode: SaveMode::Copy,
    }));
    assert_eq!(
        live.shell("cat 'notes (local copy).txt'; echo; cat 'notes (local copy 2).txt'"),
        b"v3 local\nv3 local"
    );

    // Удалённый на сервере файл — тоже конфликт, без состояния.
    live.shell("rm notes.txt");
    match live.transfer(|id| Command::UploadFile {
        id,
        local: copy.clone(),
        remote: remote.clone(),
        mode: SaveMode::IfUnchanged(saved.stamp),
    }) {
        TransferOutcome::Conflict { current } => assert_eq!(current, None),
        other => panic!("ожидался конфликт: {other:?}"),
    }
    // «Перезаписать» создаёт его заново.
    completed(live.transfer(|id| Command::UploadFile {
        id,
        local: copy.clone(),
        remote: remote.clone(),
        mode: SaveMode::Overwrite,
    }));
    assert_eq!(live.shell("cat notes.txt"), b"v3 local");
}

#[test]
#[ignore = "нужен SSHDECK_TEST_SSH"]
fn live_cancel_leaves_no_partial_file() {
    let live = Live::start("cancel");
    // Разреженный файл: на сервере создаётся мгновенно, а скачивается
    // заведомо дольше, чем идёт отмена.
    live.shell("truncate -s 4G big");
    let local = tempfile::tempdir().expect("tempdir");
    let id = live.next_id.get();
    live.next_id.set(id + 1);
    live.send(Command::Download {
        id,
        sources: vec![live.path("big")],
        dest_dir: local.path().to_path_buf(),
    });
    // Ждём первых байтов и отменяем.
    live.wait(|event| match event {
        Event::Progress { id: p, done, .. } if p == id && done > 0 => Some(()),
        _ => None,
    });
    live.send(Command::Cancel { id });
    let outcome = live.wait(|event| match event {
        Event::TransferDone { id: done, outcome } if done == id => Some(outcome),
        _ => None,
    });
    assert_eq!(outcome, TransferOutcome::Cancelled);
    assert_eq!(std::fs::read_dir(local.path()).expect("dir").count(), 0);
    // Соединение живо после брошенных запросов.
    assert!(live.list(&live.dir).iter().any(|e| e.name == "big"));
}
