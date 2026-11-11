//! SFTP-бэкенд панели файлов.
//!
//! Работает в отдельном tokio-рантайме и не зависит от GTK. Интерфейс
//! отправляет [`Command`] через [`SftpWorker::send`] и получает [`Event`] из
//! канала. Соединение не аутентифицируется само: дочерний
//! `ssh -S <сокет> -s <хост> sftp` идёт через мастер, поднятый терминалом.

mod transfer;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use async_channel::Sender;
use futures_util::StreamExt;
use futures_util::stream;
use openssh_sftp_client::error::SftpErrorKind;
use openssh_sftp_client::{Error as SftpError, Sftp, SftpOptions};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command as ProcCommand};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::session::Session;

/// Идентификатор передачи; назначается интерфейсом.
pub type TransferId = u64;

/// Как часто проверять, поднялся ли мастер.
const MASTER_POLL: Duration = Duration::from_millis(700);
const MASTER_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RETRY_DELAY_MIN: Duration = Duration::from_millis(1500);
const RETRY_DELAY_MAX: Duration = Duration::from_secs(10);
/// Сколько символических ссылок проверять одновременно при чтении каталога.
const LINK_STAT_CONCURRENCY: usize = 16;
const STDERR_LIMIT: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    /// Сокеты, устройства, битые ссылки и прочее, что нельзя ни открыть,
    /// ни раскрыть.
    Other,
}

/// Элемент удалённого каталога.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    /// Полный путь на сервере.
    pub path: String,
    /// Тип с учётом перехода по ссылке.
    pub kind: EntryKind,
    pub is_link: bool,
    pub size: Option<u64>,
    /// Время изменения, секунды Unix.
    pub modified: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    /// Мастер-соединение ещё не поднято (или уже закрыто).
    Waiting { last_error: Option<String> },
    /// Мастер есть, запускается SFTP-подсистема.
    Connecting,
    Connected { home: String },
}

#[derive(Debug)]
pub enum Command {
    /// Перейти в каталог: путь нормализуется сервером и читается.
    Navigate { path: String },
    /// Прочитать каталог (подгрузка узла дерева или обновление).
    List { path: String },
    Mkdir { path: String },
    Rename { from: String, to: String },
    /// Удалить файлы и каталоги (каталоги — рекурсивно).
    Delete { paths: Vec<String> },
    /// Скачать файлы и каталоги в локальный каталог `dest_dir`.
    Download {
        id: TransferId,
        sources: Vec<String>,
        dest_dir: PathBuf,
    },
    /// Загрузить локальные файлы и каталоги в удалённый каталог `dest_dir`.
    Upload {
        id: TransferId,
        sources: Vec<PathBuf>,
        dest_dir: String,
    },
    /// Залить один файл по точному удалённому пути (сохранение из редактора).
    UploadFile {
        id: TransferId,
        local: PathBuf,
        remote: String,
    },
    Cancel { id: TransferId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferOutcome {
    /// Готово; `skipped` — то, что не передавалось (ссылки на каталоги,
    /// сокеты и т. п.).
    Completed { skipped: Vec<String> },
    Cancelled,
    Failed(String),
}

#[derive(Debug)]
pub enum Event {
    State(ConnState),
    Navigated {
        requested: String,
        /// Нормализованный путь и содержимое.
        result: Result<(String, Vec<RemoteEntry>), String>,
    },
    Listing {
        path: String,
        result: Result<Vec<RemoteEntry>, String>,
    },
    /// Завершилась операция создания, переименования или удаления.
    OpDone {
        error: Option<String>,
        /// Каталоги, содержимое которых могло измениться.
        refresh: Vec<String>,
    },
    Progress {
        id: TransferId,
        done: u64,
        total: u64,
        current: String,
    },
    TransferDone {
        id: TransferId,
        outcome: TransferOutcome,
    },
}

/// Общий рантайм для всех вкладок: сетевой ввод-вывод живёт в его потоках,
/// а не в потоке GTK.
fn runtime() -> io::Result<&'static Runtime> {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    if let Some(rt) = RUNTIME.get() {
        return Ok(rt);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("sshdeck-sftp")
        .enable_all()
        .build()?;
    Ok(RUNTIME.get_or_init(|| rt))
}

/// Ручка фоновой задачи одной вкладки. Когда ручка уничтожается, задача
/// завершает дочерний ssh и останавливается.
pub struct SftpWorker {
    tx: mpsc::UnboundedSender<Command>,
}

impl SftpWorker {
    pub fn start(session: Session, events: Sender<Event>) -> io::Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        runtime()?.spawn(run(session, rx, events));
        Ok(Self { tx })
    }

    pub fn send(&self, command: Command) {
        // Ошибка означает, что задача уже остановлена вместе с вкладкой.
        let _ = self.tx.send(command);
    }
}

fn emit(events: &Sender<Event>, event: Event) {
    // Канал неограниченный; ошибка возможна, только если вкладка закрыта.
    let _ = events.try_send(event);
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Текст ошибки вместе с цепочкой причин. Типовые ответы SFTP-сервера
/// переводятся в короткие понятные фразы.
pub(crate) fn error_text(err: &(dyn std::error::Error + 'static)) -> String {
    if let Some(SftpError::SftpError(kind, message)) = err.downcast_ref::<SftpError>() {
        return match kind {
            SftpErrorKind::NoSuchFile => "нет такого файла или каталога".to_string(),
            SftpErrorKind::PermDenied => "недостаточно прав".to_string(),
            SftpErrorKind::OpUnsupported => "сервер не поддерживает эту операцию".to_string(),
            // «Failure» — общий отказ без подробностей: каталог не пуст, имя
            // занято, кончилось место и т. п.
            SftpErrorKind::Failure => "сервер отклонил операцию".to_string(),
            _ => format!("ошибка сервера: {}", message.get().0),
        };
    }
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// Склеивает путь каталога и имя.
pub fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Родительский каталог удалённого пути (`/` для верхнего уровня).
pub fn parent_remote(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(idx) => trimmed[..idx].to_string(),
    }
}

/// Последний компонент удалённого пути.
pub fn base_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

async fn run(session: Session, mut rx: mpsc::UnboundedReceiver<Command>, events: Sender<Event>) {
    let mut last_error: Option<String> = None;
    let mut retry_delay = RETRY_DELAY_MIN;
    loop {
        emit(
            &events,
            Event::State(ConnState::Waiting {
                last_error: last_error.clone(),
            }),
        );
        while !master_alive(&session).await {
            if !idle(&mut rx, &events, MASTER_POLL).await {
                return;
            }
        }

        emit(&events, Event::State(ConnState::Connecting));
        match connect(&session).await {
            Ok((connection, home)) => {
                retry_delay = RETRY_DELAY_MIN;
                emit(&events, Event::State(ConnState::Connected { home }));
                match serve(connection, &mut rx, &events).await {
                    ServeEnd::Closed => return,
                    ServeEnd::Lost(message) => last_error = Some(message),
                }
            }
            Err(message) => {
                last_error = Some(message);
                retry_delay = (retry_delay * 2).min(RETRY_DELAY_MAX);
            }
        }
        emit(
            &events,
            Event::State(ConnState::Waiting {
                last_error: last_error.clone(),
            }),
        );
        if !idle(&mut rx, &events, retry_delay).await {
            return;
        }
    }
}

/// Ждёт `duration`, отвечая отказом на команды, пришедшие без соединения.
/// Возвращает `false`, если вкладка закрыта.
async fn idle(
    rx: &mut mpsc::UnboundedReceiver<Command>,
    events: &Sender<Event>,
    duration: Duration,
) -> bool {
    let sleep = tokio::time::sleep(duration);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return true,
            command = rx.recv() => match command {
                Some(command) => reject(command, events),
                None => return false,
            },
        }
    }
}

fn reject(command: Command, events: &Sender<Event>) {
    let message = "Нет SFTP-соединения".to_string();
    let event = match command {
        Command::Navigate { path } => Event::Navigated {
            requested: path,
            result: Err(message),
        },
        Command::List { path } => Event::Listing {
            path,
            result: Err(message),
        },
        Command::Mkdir { .. } | Command::Rename { .. } | Command::Delete { .. } => Event::OpDone {
            error: Some(message),
            refresh: Vec::new(),
        },
        Command::Download { id, .. }
        | Command::Upload { id, .. }
        | Command::UploadFile { id, .. } => Event::TransferDone {
            id,
            outcome: TransferOutcome::Failed(message),
        },
        Command::Cancel { .. } => return,
    };
    emit(events, event);
}

async fn master_alive(session: &Session) -> bool {
    if !session.control_path().exists() {
        return false;
    }
    let argv = session.check_argv();
    let status = ProcCommand::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    matches!(
        tokio::time::timeout(MASTER_CHECK_TIMEOUT, status).await,
        Ok(Ok(status)) if status.success()
    )
}

struct Connection {
    sftp: Arc<Sftp>,
    child: Child,
    stderr: Arc<Mutex<String>>,
}

async fn connect(session: &Session) -> Result<(Connection, String), String> {
    let argv = session.sftp_argv();
    let mut child = ProcCommand::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| format!("Не удалось запустить ssh: {err}"))?;

    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err("Не удалось получить каналы ввода-вывода ssh".to_string());
    };

    let stderr = Arc::new(Mutex::new(String::new()));
    if let Some(mut pipe) = child.stderr.take() {
        let sink = Arc::clone(&stderr);
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut text = lock(&sink);
                if text.len() < STDERR_LIMIT {
                    text.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
        });
    }

    let opened = tokio::time::timeout(
        CONNECT_TIMEOUT,
        Sftp::new(stdin, stdout, SftpOptions::default()),
    )
    .await;
    let sftp = match opened {
        Ok(Ok(sftp)) => sftp,
        Ok(Err(err)) => {
            // Причину обычно называет сам ssh; внутренняя ошибка протокола
            // («unexpected end of file») полезна, только если он промолчал.
            let reported = with_stderr(String::new(), &stderr).await;
            let detail = if reported.is_empty() {
                error_text(&err)
            } else {
                reported
            };
            return Err(format!("Не удалось открыть SFTP-сессию: {detail}"));
        }
        Err(_) => {
            return Err(with_stderr("Сервер не ответил на запрос SFTP".to_string(), &stderr).await);
        }
    };

    let home = match sftp.fs().canonicalize(".").await {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(err) => return Err(with_stderr(error_text(&err), &stderr).await),
    };

    Ok((
        Connection {
            sftp: Arc::new(sftp),
            child,
            stderr,
        },
        home,
    ))
}

/// Дополняет сообщение тем, что ssh написал в stderr.
async fn with_stderr(message: String, stderr: &Mutex<String>) -> String {
    // Даём задаче чтения дочитать вывод завершившегося процесса.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let text = lock(stderr).trim().to_string();
    if text.is_empty() {
        message
    } else if message.is_empty() {
        text
    } else {
        format!("{message} ({text})")
    }
}

enum ServeEnd {
    /// Вкладка закрыта.
    Closed,
    /// Дочерний ssh завершился: соединение потеряно.
    Lost(String),
}

/// Активные передачи и признак потери соединения.
#[derive(Default)]
struct Transfers {
    tokens: Mutex<HashMap<TransferId, CancellationToken>>,
    lost: AtomicBool,
}

async fn serve(
    connection: Connection,
    rx: &mut mpsc::UnboundedReceiver<Command>,
    events: &Sender<Event>,
) -> ServeEnd {
    let Connection {
        sftp,
        mut child,
        stderr,
    } = connection;
    let transfers = Arc::new(Transfers::default());

    let end = loop {
        tokio::select! {
            command = rx.recv() => match command {
                Some(command) => dispatch(command, &sftp, events, &transfers),
                None => break ServeEnd::Closed,
            },
            status = child.wait() => {
                let base = match status {
                    Ok(status) => format!("ssh завершился ({status})"),
                    Err(err) => format!("ssh завершился: {err}"),
                };
                break ServeEnd::Lost(with_stderr(base, &stderr).await);
            }
        }
    };

    if matches!(end, ServeEnd::Lost(_)) {
        transfers.lost.store(true, Ordering::SeqCst);
    }
    for token in lock(&transfers.tokens).values() {
        token.cancel();
    }
    drop(sftp);
    let _ = child.start_kill();
    end
}

fn dispatch(command: Command, sftp: &Arc<Sftp>, events: &Sender<Event>, transfers: &Arc<Transfers>) {
    let sftp = Arc::clone(sftp);
    let events = events.clone();
    match command {
        Command::Navigate { path } => {
            tokio::spawn(async move {
                let result = navigate(&sftp, &path).await.map_err(|e| error_text(&e));
                emit(
                    &events,
                    Event::Navigated {
                        requested: path,
                        result,
                    },
                );
            });
        }
        Command::List { path } => {
            tokio::spawn(async move {
                let result = list_dir(&sftp, &path).await.map_err(|e| error_text(&e));
                emit(&events, Event::Listing { path, result });
            });
        }
        Command::Mkdir { path } => {
            tokio::spawn(async move {
                let error = sftp.fs().create_dir(&path).await.err().map(|e| {
                    format!(
                        "Не удалось создать каталог «{}»: {}",
                        base_name(&path),
                        error_text(&e)
                    )
                });
                emit(
                    &events,
                    Event::OpDone {
                        error,
                        refresh: vec![parent_remote(&path)],
                    },
                );
            });
        }
        Command::Rename { from, to } => {
            tokio::spawn(async move {
                let error = rename(&sftp, &from, &to)
                    .await
                    .err()
                    .map(|e| format!("Не удалось переименовать «{}»: {e}", base_name(&from)));
                let mut refresh = vec![parent_remote(&from)];
                let to_parent = parent_remote(&to);
                if !refresh.contains(&to_parent) {
                    refresh.push(to_parent);
                }
                emit(&events, Event::OpDone { error, refresh });
            });
        }
        Command::Delete { paths } => {
            tokio::spawn(async move {
                let mut error = None;
                let mut refresh: Vec<String> = Vec::new();
                for path in &paths {
                    let parent = parent_remote(path);
                    if !refresh.contains(&parent) {
                        refresh.push(parent);
                    }
                    if let Err(err) = remove_recursive(&sftp, path).await {
                        error = Some(format!(
                            "Не удалось удалить «{}»: {}",
                            base_name(path),
                            error_text(&err)
                        ));
                        break;
                    }
                }
                emit(&events, Event::OpDone { error, refresh });
            });
        }
        Command::Download {
            id,
            sources,
            dest_dir,
        } => spawn_transfer(id, sftp, events, transfers, move |job| {
            Box::pin(transfer::download(job, sources, dest_dir))
        }),
        Command::Upload {
            id,
            sources,
            dest_dir,
        } => spawn_transfer(id, sftp, events, transfers, move |job| {
            Box::pin(transfer::upload(job, sources, dest_dir))
        }),
        Command::UploadFile { id, local, remote } => {
            spawn_transfer(id, sftp, events, transfers, move |job| {
                Box::pin(transfer::upload_file(job, local, remote))
            })
        }
        Command::Cancel { id } => {
            if let Some(token) = lock(&transfers.tokens).get(&id) {
                token.cancel();
            }
        }
    }
}

type TransferFuture<'a> =
    std::pin::Pin<Box<dyn Future<Output = Result<(), transfer::Abort>> + Send + 'a>>;

fn spawn_transfer(
    id: TransferId,
    sftp: Arc<Sftp>,
    events: Sender<Event>,
    transfers: &Arc<Transfers>,
    work: impl for<'a> FnOnce(&'a mut transfer::Job) -> TransferFuture<'a> + Send + 'static,
) {
    let token = CancellationToken::new();
    lock(&transfers.tokens).insert(id, token.clone());
    let transfers = Arc::clone(transfers);
    tokio::spawn(async move {
        let mut job = transfer::Job::new(sftp, events.clone(), id, token);
        let result = work(&mut job).await;
        lock(&transfers.tokens).remove(&id);
        let outcome = match result {
            Ok(()) => TransferOutcome::Completed {
                skipped: std::mem::take(&mut job.skipped),
            },
            Err(transfer::Abort::Cancelled) if transfers.lost.load(Ordering::SeqCst) => {
                TransferOutcome::Failed("Соединение потеряно".to_string())
            }
            Err(transfer::Abort::Cancelled) => TransferOutcome::Cancelled,
            Err(transfer::Abort::Failed(message)) => TransferOutcome::Failed(message),
        };
        emit(&events, Event::TransferDone { id, outcome });
    });
}

async fn navigate(sftp: &Sftp, path: &str) -> Result<(String, Vec<RemoteEntry>), SftpError> {
    let canonical = sftp.fs().canonicalize(path).await?;
    let canonical = canonical.to_string_lossy().into_owned();
    let entries = list_dir(sftp, &canonical).await?;
    Ok((canonical, entries))
}

/// Читает каталог. Для символических ссылок дополнительно выясняется, на
/// что они указывают, чтобы ссылку на каталог можно было раскрыть.
pub(crate) async fn list_dir(sftp: &Sftp, path: &str) -> Result<Vec<RemoteEntry>, SftpError> {
    let dir = sftp.fs().open_dir(path).await?;
    let read_dir = dir.read_dir();
    tokio::pin!(read_dir);

    let mut entries = Vec::new();
    while let Some(item) = read_dir.next().await {
        let item = item?;
        let name = item.filename().to_string_lossy().into_owned();
        if name == "." || name == ".." {
            continue;
        }
        let meta = item.metadata();
        let file_type = item.file_type();
        let is_link = file_type.is_some_and(|t| t.is_symlink());
        let kind = match file_type {
            Some(t) if t.is_dir() => EntryKind::Dir,
            Some(t) if t.is_file() => EntryKind::File,
            _ => EntryKind::Other,
        };
        entries.push(RemoteEntry {
            path: join_remote(path, &name),
            name,
            kind,
            is_link,
            size: meta.len(),
            modified: meta.modified().map(|t| t.as_duration().as_secs()),
        });
    }

    let link_targets: Vec<(usize, String)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.is_link)
        .map(|(idx, e)| (idx, e.path.clone()))
        .collect();
    let resolved: Vec<_> = stream::iter(link_targets)
        .map(|(idx, link)| async move { (idx, sftp.fs().metadata(&link).await) })
        .buffer_unordered(LINK_STAT_CONCURRENCY)
        .collect()
        .await;
    for (idx, meta) in resolved {
        // Ошибка здесь — битая ссылка; элемент остаётся с типом Other.
        if let Ok(meta) = meta {
            let entry = &mut entries[idx];
            entry.kind = match meta.file_type() {
                Some(t) if t.is_dir() => EntryKind::Dir,
                Some(t) if t.is_file() => EntryKind::File,
                _ => EntryKind::Other,
            };
            entry.size = meta.len();
            entry.modified = meta.modified().map(|t| t.as_duration().as_secs());
        }
    }
    Ok(entries)
}

/// Переименование без молчаливой замены: `posix-rename` перезаписал бы
/// существующий файл, поэтому занятость имени проверяется заранее.
async fn rename(sftp: &Sftp, from: &str, to: &str) -> Result<(), String> {
    let mut fs = sftp.fs();
    if fs.symlink_metadata(to).await.is_ok() {
        return Err(format!("имя «{}» уже занято", base_name(to)));
    }
    fs.rename(from, to).await.map_err(|e| error_text(&e))
}

/// Удаляет файл или каталог со всем содержимым. По ссылкам не переходит:
/// удаляется сама ссылка.
async fn remove_recursive(sftp: &Sftp, path: &str) -> Result<(), SftpError> {
    let mut fs = sftp.fs();
    let meta = fs.symlink_metadata(path).await?;
    if !meta.file_type().is_some_and(|t| t.is_dir()) {
        return fs.remove_file(path).await;
    }

    // Обход без рекурсии: сначала собираем каталоги, затем удаляем их в
    // обратном порядке, когда они уже пусты.
    let mut pending = vec![path.to_string()];
    let mut dirs = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in list_dir(sftp, &dir).await? {
            if entry.kind == EntryKind::Dir && !entry.is_link {
                pending.push(entry.path);
            } else {
                fs.remove_file(&entry.path).await?;
            }
        }
        dirs.push(dir);
    }
    for dir in dirs.iter().rev() {
        fs.remove_dir(dir).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_path_helpers() {
        assert_eq!(join_remote("/", "etc"), "/etc");
        assert_eq!(join_remote("/home/u", "f.txt"), "/home/u/f.txt");
        assert_eq!(parent_remote("/home/u/f.txt"), "/home/u");
        assert_eq!(parent_remote("/home/u/"), "/home");
        assert_eq!(parent_remote("/etc"), "/");
        assert_eq!(parent_remote("/"), "/");
        assert_eq!(base_name("/home/u/f.txt"), "f.txt");
        assert_eq!(base_name("/home/u/"), "u");
    }
}
