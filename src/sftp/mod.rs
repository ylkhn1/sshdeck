//! SFTP-бэкенд панели файлов.
//!
//! Работает в отдельном tokio-рантайме и не зависит от GTK. Интерфейс
//! отправляет [`Command`] через [`SftpWorker::send`] и получает [`Event`] из
//! канала. Соединение не аутентифицируется само: дочерний
//! `ssh -S <сокет> -s <хост> sftp` идёт через мастер, поднятый терминалом.

mod proto;
mod transfer;
#[cfg(test)]
mod live_tests;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use async_channel::Sender;
use futures_util::StreamExt;
use futures_util::stream;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command as ProcCommand};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::session::Session;
use proto::{Attrs, Client};

pub use crate::entry::{EntryKind, FileEntry};
pub use transfer::copy_name;

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
/// Глубже этого каталоги не обходятся (при скачивании, загрузке и
/// удалении): защита от бесконечных деревьев враждебного сервера.
const MAX_TREE_DEPTH: usize = 256;
/// Больше стольких элементов за одну операцию не обходим.
const MAX_TREE_ENTRIES: usize = 1_000_000;
/// Сколько ждать, пока ssh завершится сам, после закрытия соединения.
const EXIT_GRACE: Duration = Duration::from_millis(500);

/// Что известно о версии файла на сервере: по этому видно, менялся ли он.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteStamp {
    pub size: Option<u64>,
    pub modified: Option<u64>,
}

impl RemoteStamp {
    fn of(attrs: &Attrs) -> Self {
        Self {
            size: attrs.size,
            modified: attrs.modified().map(u64::from),
        }
    }
}

/// Файл, который передавался одиночной передачей, и его состояние на
/// сервере после неё.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    pub path: PathBuf,
    pub stamp: RemoteStamp,
}

/// Как сохранять файл из редактора.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveMode {
    /// Залить, только если файл на сервере всё ещё в этом состоянии;
    /// иначе передача завершается [`TransferOutcome::Conflict`].
    IfUnchanged(RemoteStamp),
    /// Залить поверх, не проверяя.
    Overwrite,
    /// Сохранить рядом под свободным именем вида «имя (local copy).ext».
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    /// Мастер-соединение ещё не поднято (или уже закрыто).
    Waiting { last_error: Option<String> },
    /// Мастер есть, запускается SFTP-подсистема.
    Connecting,
    Connected { home: PathBuf },
}

#[derive(Debug)]
pub enum Command {
    /// Перейти в каталог: путь нормализуется сервером и читается.
    Navigate { path: PathBuf },
    /// Прочитать каталог (подгрузка узла дерева или обновление).
    List { path: PathBuf },
    Mkdir { path: PathBuf },
    Rename { from: PathBuf, to: PathBuf },
    /// Удалить файлы и каталоги (каталоги — рекурсивно).
    Delete { paths: Vec<PathBuf> },
    /// Скачать файлы и каталоги в локальный каталог `dest_dir`.
    Download {
        id: TransferId,
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    },
    /// Загрузить локальные файлы и каталоги в удалённый каталог `dest_dir`.
    Upload {
        id: TransferId,
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    },
    /// Залить один файл по точному удалённому пути (сохранение из редактора).
    UploadFile {
        id: TransferId,
        local: PathBuf,
        remote: PathBuf,
        mode: SaveMode,
    },
    Cancel { id: TransferId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferOutcome {
    /// Готово. `skipped` — то, что не передавалось (спецфайлы, битые и
    /// зацикленные ссылки, слишком глубокие каталоги). `size_mismatch` —
    /// скачанные файлы, размер которых не совпал с тем, что сообщил сервер
    /// (файл менялся во время скачивания или сервер ведёт себя странно).
    /// `file` — итог одиночной передачи файла: скачивания одного файла или
    /// сохранения из редактора.
    Completed {
        skipped: Vec<PathBuf>,
        size_mismatch: Vec<PathBuf>,
        file: Option<RemoteFile>,
    },
    /// Файл на сервере изменился после скачивания (`None` — его удалили);
    /// ничего не залито.
    Conflict { current: Option<RemoteStamp> },
    Cancelled,
    Failed(String),
}

#[derive(Debug)]
pub enum Event {
    State(ConnState),
    Navigated {
        requested: PathBuf,
        /// Нормализованный путь и содержимое.
        result: Result<(PathBuf, Vec<FileEntry>), String>,
    },
    Listing {
        path: PathBuf,
        result: Result<Vec<FileEntry>, String>,
    },
    /// Завершилась операция создания, переименования или удаления.
    OpDone {
        error: Option<String>,
        /// Каталоги, содержимое которых могло измениться.
        refresh: Vec<PathBuf>,
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
    if let Some(err) = err.downcast_ref::<proto::Error>() {
        return match err {
            proto::Error::Status { code, .. } => match *code {
                proto::FX_NO_SUCH_FILE => tr!("no such file or folder").to_string(),
                proto::FX_PERMISSION_DENIED => tr!("permission denied").to_string(),
                proto::FX_OP_UNSUPPORTED => tr!("the server does not support this operation").to_string(),
                // «Failure» — общий отказ без подробностей: каталог не пуст, имя
                // занято, кончилось место и т. п.
                proto::FX_FAILURE => tr!("the server rejected the operation").to_string(),
                _ => tr!("server error: {}", err),
            },
            proto::Error::Lost => tr!("Connection lost").to_string(),
            proto::Error::Protocol(_) => tr!("server error: {}", err),
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

/// Удалённый путь для показа: недопустимые в UTF-8 байты заменяются на «�».
pub fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn trim_slashes(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 1 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    &bytes[..end]
}

/// Склеивает путь каталога и имя (байтами, без требований к кодировке).
pub fn join_remote(dir: &Path, name: impl AsRef<OsStr>) -> PathBuf {
    let mut bytes = dir.as_os_str().as_bytes().to_vec();
    if !bytes.ends_with(b"/") {
        bytes.push(b'/');
    }
    bytes.extend_from_slice(name.as_ref().as_bytes());
    PathBuf::from(OsString::from_vec(bytes))
}

/// Родительский каталог удалённого пути (`/` для верхнего уровня).
pub fn parent_remote(path: &Path) -> PathBuf {
    let trimmed = trim_slashes(path.as_os_str().as_bytes());
    match trimmed.iter().rposition(|&b| b == b'/') {
        Some(0) | None => PathBuf::from("/"),
        Some(idx) => PathBuf::from(OsStr::from_bytes(&trimmed[..idx])),
    }
}

/// Последний компонент удалённого пути (пустой для `/`).
pub fn base_name(path: &Path) -> &OsStr {
    let trimmed = trim_slashes(path.as_os_str().as_bytes());
    let start = trimmed.iter().rposition(|&b| b == b'/').map_or(0, |idx| idx + 1);
    OsStr::from_bytes(&trimmed[start..])
}

/// Имя, пришедшее с сервера, годится в путь: ровно один обычный компонент
/// (не пустое, не `.`/`..`, без `/` и нулевых байтов).
pub fn is_plain_name(name: &OsStr) -> bool {
    if !proto::valid_name(name.as_bytes()) {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// Удалённый путь элемента каталога; `None` для недопустимого имени.
pub fn remote_child(dir: &Path, name: &OsStr) -> Option<PathBuf> {
    is_plain_name(name).then(|| join_remote(dir, name))
}

/// Локальный путь для имени с сервера; `None` для недопустимого имени.
/// Без этой проверки абсолютное имя («/home/u/.bashrc») в `Path::join`
/// заменило бы весь путь, а «../x» вышло бы из каталога назначения.
pub fn local_child(dir: &Path, name: &OsStr) -> Option<PathBuf> {
    is_plain_name(name).then(|| dir.join(name))
}

/// Лежит ли `path` внутри каталога `dir` (на любой глубине).
pub fn is_inside(path: &Path, dir: &Path) -> bool {
    let path = path.as_os_str().as_bytes();
    let dir = trim_slashes(dir.as_os_str().as_bytes());
    if dir == b"/" {
        return path.len() > 1 && path.starts_with(b"/");
    }
    path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
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
    let message = tr!("No SFTP connection").to_string();
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
    sftp: Client,
    child: Child,
    stderr: Arc<Mutex<String>>,
}

async fn connect(session: &Session) -> Result<(Connection, PathBuf), String> {
    let argv = session.sftp_argv();
    let mut child = ProcCommand::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| tr!("Could not start ssh: {}", err))?;

    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(tr!("Could not get the ssh input/output pipes").to_string());
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

    // В общий срок входят и приветствие, и запрос домашнего каталога.
    let opened = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let sftp = Client::start(stdout, stdin).await?;
        let home = sftp.realpath(Path::new(".")).await;
        Ok::<_, proto::Error>((sftp, home))
    })
    .await;
    let (sftp, home) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err(err)) => {
            // Причину обычно называет сам ssh; внутренняя ошибка протокола
            // («соединение закрыто») полезна, только если он промолчал.
            let reported = with_stderr(String::new(), &stderr).await;
            let detail = if reported.is_empty() {
                error_text(&err)
            } else {
                reported
            };
            return Err(tr!("Could not open an SFTP session: {}", detail));
        }
        Err(_) => {
            return Err(with_stderr(tr!("The server did not respond to the SFTP request").to_string(), &stderr).await);
        }
    };

    let home = match home {
        Ok(path) => path,
        Err(err) => return Err(with_stderr(error_text(&err), &stderr).await),
    };

    Ok((
        Connection {
            sftp,
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
    let closed = sftp.closed();
    tokio::pin!(closed);

    let end = loop {
        tokio::select! {
            command = rx.recv() => match command {
                Some(command) => dispatch(command, &sftp, events, &transfers),
                None => break ServeEnd::Closed,
            },
            status = child.wait() => {
                break ServeEnd::Lost(with_stderr(exit_text(status), &stderr).await);
            }
            // Соединение сломалось, а ssh ещё жив (нарушение протокола,
            // зависший сервер): уходим на обычный путь переподключения.
            () = &mut closed => {
                let base = match tokio::time::timeout(EXIT_GRACE, child.wait()).await {
                    Ok(status) => exit_text(status),
                    Err(_) => tr!(
                        "The SFTP connection failed: {}",
                        sftp.close_reason().unwrap_or_default()
                    ),
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

fn exit_text(status: io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(status) => tr!("ssh exited ({})", status),
        Err(err) => tr!("ssh exited: {}", err),
    }
}

fn dispatch(command: Command, sftp: &Client, events: &Sender<Event>, transfers: &Arc<Transfers>) {
    let sftp = sftp.clone();
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
                let error = sftp.mkdir(&path).await.err().map(|e| {
                    tr!(
                        "Could not create folder “{}”: {}",
                        display(Path::new(base_name(&path))),
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
                let error = rename(&sftp, &from, &to).await.err().map(|e| {
                    tr!(
                        "Could not rename “{}”: {}",
                        display(Path::new(base_name(&from))),
                        e
                    )
                });
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
                let mut refresh: Vec<PathBuf> = Vec::new();
                for path in &paths {
                    let parent = parent_remote(path);
                    if !refresh.contains(&parent) {
                        refresh.push(parent);
                    }
                    if let Err(err) = remove_recursive(&sftp, path).await {
                        error = Some(tr!(
                            "Could not delete “{}”: {}",
                            display(Path::new(base_name(path))),
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
        Command::UploadFile {
            id,
            local,
            remote,
            mode,
        } => spawn_transfer(id, sftp, events, transfers, move |job| {
            Box::pin(transfer::upload_file(job, local, remote, mode))
        }),
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
    sftp: Client,
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
                size_mismatch: std::mem::take(&mut job.size_mismatch),
                file: job.file.take(),
            },
            Err(transfer::Abort::Cancelled) if transfers.lost.load(Ordering::SeqCst) => {
                TransferOutcome::Failed(tr!("Connection lost").to_string())
            }
            Err(transfer::Abort::Cancelled) => TransferOutcome::Cancelled,
            Err(transfer::Abort::Conflict(current)) => TransferOutcome::Conflict { current },
            Err(transfer::Abort::Failed(message)) => TransferOutcome::Failed(message),
        };
        emit(&events, Event::TransferDone { id, outcome });
    });
}

async fn navigate(sftp: &Client, path: &Path) -> Result<(PathBuf, Vec<FileEntry>), proto::Error> {
    let canonical = sftp.realpath(path).await?;
    let entries = list_dir(sftp, &canonical).await?;
    Ok((canonical, entries))
}

fn kind_of(attrs: &Attrs) -> EntryKind {
    if attrs.is_dir() {
        EntryKind::Dir
    } else if attrs.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

/// Читает каталог. Для символических ссылок дополнительно выясняется, на
/// что они указывают, чтобы ссылку на каталог можно было раскрыть.
pub(crate) async fn list_dir(sftp: &Client, path: &Path) -> Result<Vec<FileEntry>, proto::Error> {
    // Недопустимые имена уже отброшены при чтении; проверка пути — ещё
    // один рубеж.
    let mut entries: Vec<FileEntry> = sftp
        .read_dir(path)
        .await?
        .into_iter()
        .filter_map(|(name, attrs)| {
            Some(FileEntry {
                path: remote_child(path, &name)?,
                name,
                kind: kind_of(&attrs),
                is_link: attrs.is_symlink(),
                size: attrs.size,
                modified: attrs.modified().map(u64::from),
                mode: attrs.mode(),
            })
        })
        .collect();

    let link_targets: Vec<(usize, PathBuf)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.is_link)
        .map(|(idx, e)| (idx, e.path.clone()))
        .collect();
    let resolved: Vec<_> = stream::iter(link_targets)
        .map(|(idx, link)| async move { (idx, sftp.stat(&link).await) })
        .buffer_unordered(LINK_STAT_CONCURRENCY)
        .collect()
        .await;
    for (idx, attrs) in resolved {
        // Ошибка здесь — битая ссылка; элемент остаётся с типом Other.
        if let Ok(attrs) = attrs {
            let entry = &mut entries[idx];
            entry.kind = kind_of(&attrs);
            entry.size = attrs.size;
            entry.modified = attrs.modified().map(u64::from);
            entry.mode = attrs.mode();
        }
    }
    Ok(entries)
}

/// Переименование без молчаливой замены: занятость имени проверяется
/// заранее (сервер мог бы заменить существующий каталог).
async fn rename(sftp: &Client, from: &Path, to: &Path) -> Result<(), String> {
    if sftp.lstat(to).await.is_ok() {
        return Err(tr!(
            "the name “{}” is already taken",
            display(Path::new(base_name(to)))
        ));
    }
    sftp.rename(from, to).await.map_err(|e| error_text(&e))
}

/// Удаляет файл или каталог со всем содержимым. По ссылкам не переходит:
/// удаляется сама ссылка.
async fn remove_recursive(sftp: &Client, path: &Path) -> Result<(), proto::Error> {
    let attrs = sftp.lstat(path).await?;
    if !attrs.is_dir() {
        return sftp.remove(path).await;
    }

    // Обход без рекурсии: сначала собираем каталоги, затем удаляем их в
    // обратном порядке, когда они уже пусты. Глубина и число элементов
    // ограничены: дерево на враждебном сервере может быть бесконечным.
    let too_big = || proto::Error::Protocol("the folder tree is too large".to_string());
    let mut pending = vec![(path.to_path_buf(), 0usize)];
    let mut dirs = Vec::new();
    let mut seen = 0usize;
    while let Some((dir, depth)) = pending.pop() {
        for entry in list_dir(sftp, &dir).await? {
            seen += 1;
            if seen > MAX_TREE_ENTRIES {
                return Err(too_big());
            }
            let Some(child) = remote_child(&dir, &entry.name) else {
                continue;
            };
            if entry.kind == EntryKind::Dir && !entry.is_link {
                if depth + 1 >= MAX_TREE_DEPTH {
                    return Err(too_big());
                }
                pending.push((child, depth + 1));
            } else {
                sftp.remove(&child).await?;
            }
        }
        dirs.push(dir);
    }
    for dir in dirs.iter().rev() {
        sftp.rmdir(dir).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(bytes: &[u8]) -> PathBuf {
        PathBuf::from(OsString::from_vec(bytes.to_vec()))
    }

    #[test]
    fn remote_path_helpers() {
        assert_eq!(join_remote(Path::new("/"), "etc"), p(b"/etc"));
        assert_eq!(join_remote(Path::new("/home/u"), "f.txt"), p(b"/home/u/f.txt"));
        assert_eq!(parent_remote(Path::new("/home/u/f.txt")), p(b"/home/u"));
        assert_eq!(parent_remote(Path::new("/home/u/")), p(b"/home"));
        assert_eq!(parent_remote(Path::new("/etc")), p(b"/"));
        assert_eq!(parent_remote(Path::new("/")), p(b"/"));
        assert_eq!(base_name(Path::new("/home/u/f.txt")), "f.txt");
        assert_eq!(base_name(Path::new("/home/u/")), "u");
        assert_eq!(base_name(Path::new("/")), "");
    }

    #[test]
    fn remote_paths_keep_raw_bytes() {
        let dir = p(b"/srv/\xff\xfe");
        let file = join_remote(&dir, OsStr::from_bytes(b"caf\xe9.txt"));
        assert_eq!(file.as_os_str().as_bytes(), b"/srv/\xff\xfe/caf\xe9.txt");
        assert_eq!(parent_remote(&file), dir);
        assert_eq!(base_name(&file).as_bytes(), b"caf\xe9.txt");
        assert_eq!(display(&file), "/srv/\u{fffd}\u{fffd}/caf\u{fffd}.txt");
        let entry = FileEntry {
            name: OsString::from_vec(b".\xff".to_vec()),
            path: p(b"/srv/.\xff"),
            kind: EntryKind::File,
            is_link: false,
            size: None,
            modified: None,
            mode: None,
        };
        assert!(entry.is_hidden());
        assert_eq!(entry.display_name(), ".\u{fffd}");
    }

    #[test]
    fn server_names_never_escape() {
        let dir = Path::new("/home/u/dl");
        for bad in [&b"/etc/x"[..], b"../x", b"a/b", b"", b".", b"..", b"a\0b", b"/", b"x/"] {
            let name = OsStr::from_bytes(bad);
            assert!(!is_plain_name(name), "{bad:?}");
            assert_eq!(local_child(dir, name), None, "{bad:?}");
            assert_eq!(remote_child(dir, name), None, "{bad:?}");
        }
        let name = OsStr::from_bytes(b"\xff.txt");
        assert_eq!(local_child(dir, name), Some(dir.join(name)));
        assert_eq!(remote_child(dir, OsStr::new("..x")), Some(p(b"/home/u/dl/..x")));
    }

    #[test]
    fn inside_check() {
        assert!(is_inside(Path::new("/a/b"), Path::new("/a")));
        assert!(is_inside(Path::new("/a/b/c"), Path::new("/a/")));
        assert!(!is_inside(Path::new("/ab"), Path::new("/a")));
        assert!(!is_inside(Path::new("/a"), Path::new("/a")));
        assert!(is_inside(Path::new("/a"), Path::new("/")));
        assert!(!is_inside(Path::new("/"), Path::new("/")));
    }
}
