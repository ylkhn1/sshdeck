//! Небольшой клиент SFTP версии 3 поверх stdin/stdout дочернего ssh.
//!
//! Пути и имена передаются как байты (`OsStr` в Unix) и не обязаны быть
//! UTF-8: на сервере имя файла — произвольная последовательность байтов,
//! и её нужно уметь и показать, и вернуть серверу без искажений.
//!
//! Запросы конвейерные. Каждый получает свой номер и отправляется сразу,
//! а фоновая задача чтения раздаёт ответы ждущим по номеру. Поэтому
//! несколько чтений одного файла можно держать «в полёте» одновременно,
//! а брошенный (отменённый) запрос ничего не ломает: его ответ просто
//! выбрасывается.
//!
//! Сервер считается недоверенным: имена из списков каталогов проверяются
//! ([`valid_name`]), размеры ответов ограничены, бесконечные ответы и
//! зависания обрываются (сторож неактивности), а текст ошибок сервера
//! очищается от управляющих символов и укорачивается.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};

const FXP_INIT: u8 = 1;
const FXP_VERSION: u8 = 2;
const FXP_OPEN: u8 = 3;
const FXP_CLOSE: u8 = 4;
const FXP_READ: u8 = 5;
const FXP_WRITE: u8 = 6;
const FXP_LSTAT: u8 = 7;
const FXP_SETSTAT: u8 = 9;
const FXP_FSETSTAT: u8 = 10;
const FXP_OPENDIR: u8 = 11;
const FXP_READDIR: u8 = 12;
const FXP_REMOVE: u8 = 13;
const FXP_MKDIR: u8 = 14;
const FXP_RMDIR: u8 = 15;
const FXP_REALPATH: u8 = 16;
const FXP_STAT: u8 = 17;
const FXP_RENAME: u8 = 18;
const FXP_EXTENDED: u8 = 200;

const FXP_STATUS: u8 = 101;
const FXP_HANDLE: u8 = 102;
const FXP_DATA: u8 = 103;
const FXP_NAME: u8 = 104;
const FXP_ATTRS: u8 = 105;
const FXP_EXTENDED_REPLY: u8 = 201;

pub const FX_OK: u32 = 0;
pub const FX_EOF: u32 = 1;
pub const FX_NO_SUCH_FILE: u32 = 2;
pub const FX_PERMISSION_DENIED: u32 = 3;
pub const FX_FAILURE: u32 = 4;
pub const FX_OP_UNSUPPORTED: u32 = 8;

/// Флаги открытия файла.
pub const OPEN_READ: u32 = 0x01;
pub const OPEN_WRITE: u32 = 0x02;
pub const OPEN_CREATE: u32 = 0x08;
pub const OPEN_TRUNCATE: u32 = 0x10;
pub const OPEN_EXCLUSIVE: u32 = 0x20;

const ATTR_SIZE: u32 = 0x01;
const ATTR_UIDGID: u32 = 0x02;
const ATTR_PERMISSIONS: u32 = 0x04;
const ATTR_ACMODTIME: u32 = 0x08;
const ATTR_EXTENDED: u32 = 0x8000_0000;

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;

/// Самый большой принимаемый пакет. OpenSSH не шлёт больше 256 КиБ.
const MAX_PACKET: usize = 1024 * 1024;
/// Размер куска чтения и записи, который обязан поддерживать любой сервер.
const SAFE_CHUNK: u32 = 32 * 1024;
/// Верхняя граница куска, даже если сервер разрешает больше.
const MAX_CHUNK: u32 = 255 * 1024;
/// Сколько данных держать «в полёте» при конвейерной передаче.
const IN_FLIGHT_BYTES: u32 = 2 * 1024 * 1024;
/// Сколько сервер может молчать, пока есть неотвеченные запросы; потом
/// соединение считается мёртвым.
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(60);
const WATCHDOG_TICK: Duration = Duration::from_secs(5);
/// Больше стольких имён в одном каталоге не принимаем.
const MAX_DIR_ENTRIES: usize = 1_000_000;
/// Пустые ответы NAME подряд, после которых листинг считается бесконечным.
const MAX_EMPTY_NAME_REPLIES: u32 = 16;
/// Сколько символов текста ошибки сервера показывать.
const MAX_MESSAGE_CHARS: usize = 300;

/// Ошибка SFTP-запроса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Сервер ответил отказом.
    Status { code: u32, message: String },
    /// Соединение закрыто: дочерний ssh завершился.
    Lost,
    /// Сервер прислал что-то не по протоколу.
    Protocol(String),
}

impl Error {
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::Status { code: FX_NO_SUCH_FILE, .. })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Status { code, message } if message.is_empty() => {
                write!(f, "SFTP status {code}")
            }
            Error::Status { message, .. } => f.write_str(message),
            Error::Lost => f.write_str("connection closed"),
            Error::Protocol(message) => write!(f, "SFTP protocol error: {message}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Атрибуты файла (всё необязательно: сервер может что-то не прислать).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attrs {
    pub size: Option<u64>,
    pub uid_gid: Option<(u32, u32)>,
    /// Права вместе с битами типа файла.
    pub permissions: Option<u32>,
    /// Время доступа и изменения, секунды Unix.
    pub times: Option<(u32, u32)>,
}

impl Attrs {
    fn file_type(&self) -> Option<u32> {
        self.permissions.map(|mode| mode & S_IFMT)
    }

    pub fn is_dir(&self) -> bool {
        self.file_type() == Some(S_IFDIR)
    }

    pub fn is_file(&self) -> bool {
        self.file_type() == Some(S_IFREG)
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type() == Some(S_IFLNK)
    }

    pub fn modified(&self) -> Option<u32> {
        self.times.map(|(_, modified)| modified)
    }

    fn encode(&self, packet: &mut Packet) {
        let mut flags = 0;
        if self.size.is_some() {
            flags |= ATTR_SIZE;
        }
        if self.uid_gid.is_some() {
            flags |= ATTR_UIDGID;
        }
        if self.permissions.is_some() {
            flags |= ATTR_PERMISSIONS;
        }
        if self.times.is_some() {
            flags |= ATTR_ACMODTIME;
        }
        packet.u32(flags);
        if let Some(size) = self.size {
            packet.u64(size);
        }
        if let Some((uid, gid)) = self.uid_gid {
            packet.u32(uid);
            packet.u32(gid);
        }
        if let Some(mode) = self.permissions {
            packet.u32(mode);
        }
        if let Some((atime, mtime)) = self.times {
            packet.u32(atime);
            packet.u32(mtime);
        }
    }

    fn decode(reader: &mut Reader<'_>) -> Result<Self> {
        let flags = reader.u32()?;
        let mut attrs = Attrs::default();
        if flags & ATTR_SIZE != 0 {
            attrs.size = Some(reader.u64()?);
        }
        if flags & ATTR_UIDGID != 0 {
            attrs.uid_gid = Some((reader.u32()?, reader.u32()?));
        }
        if flags & ATTR_PERMISSIONS != 0 {
            attrs.permissions = Some(reader.u32()?);
        }
        if flags & ATTR_ACMODTIME != 0 {
            attrs.times = Some((reader.u32()?, reader.u32()?));
        }
        if flags & ATTR_EXTENDED != 0 {
            for _ in 0..reader.u32()? {
                reader.bytes()?;
                reader.bytes()?;
            }
        }
        Ok(attrs)
    }
}

// ------------------------------------------------------------ кодирование

/// Исходящий пакет; длина вписывается в начало в [`Packet::finish`].
struct Packet(Vec<u8>);

impl Packet {
    fn new(kind: u8) -> Self {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&[0, 0, 0, 0, kind]);
        Packet(buf)
    }

    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn bytes(&mut self, value: &[u8]) {
        // Длина строки в SFTP — u32; пакеты здесь заведомо меньше.
        self.u32(value.len() as u32);
        self.0.extend_from_slice(value);
    }

    fn path(&mut self, path: &Path) {
        self.bytes(path.as_os_str().as_bytes());
    }

    fn finish(mut self) -> Vec<u8> {
        let len = (self.0.len() - 4) as u32;
        self.0[..4].copy_from_slice(&len.to_be_bytes());
        self.0
    }
}

/// Разбор входящего пакета.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(Error::Protocol("truncated packet".to_string()));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(buf))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn string_lossy(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }
}

/// Текст сервера для показа: без управляющих символов и не длиннее
/// [`MAX_MESSAGE_CHARS`].
fn sanitize_message(text: &str) -> String {
    let mut clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_MESSAGE_CHARS + 1)
        .collect();
    if clean.chars().count() > MAX_MESSAGE_CHARS {
        clean = clean.chars().take(MAX_MESSAGE_CHARS).collect();
        clean.push('…');
    }
    clean.trim().to_string()
}

/// Допустимое имя элемента каталога: непустое, не `.` и не `..`, без `/` и
/// нулевых байтов. Всё остальное — попытка выйти за пределы каталога
/// (`/etc/passwd`, `../x`, `a/b`) или сломать пути, и такие имена
/// выбрасываются прямо при чтении каталога.
pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name != b"." && name != b".." && !name.contains(&b'/') && !name.contains(&0)
}

#[derive(Debug)]
enum Reply {
    Status { code: u32, message: String },
    Handle(Vec<u8>),
    Data(Vec<u8>),
    Name(Vec<(OsString, Attrs)>),
    Attrs(Attrs),
    Extended(Vec<u8>),
}

fn parse_reply(kind: u8, reader: &mut Reader<'_>) -> Result<Reply> {
    Ok(match kind {
        FXP_STATUS => {
            let code = reader.u32()?;
            // Текст ошибки и язык появились только в поздних редакциях v3.
            let message = if reader.0.is_empty() {
                String::new()
            } else {
                sanitize_message(&reader.string_lossy()?)
            };
            Reply::Status { code, message }
        }
        FXP_HANDLE => Reply::Handle(reader.bytes()?.to_vec()),
        FXP_DATA => Reply::Data(reader.bytes()?.to_vec()),
        FXP_NAME => {
            let count = reader.u32()? as usize;
            let mut names = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                let name = OsString::from_vec(reader.bytes()?.to_vec());
                reader.bytes()?; // longname — вывод ls, не нужен
                names.push((name, Attrs::decode(reader)?));
            }
            Reply::Name(names)
        }
        FXP_ATTRS => Reply::Attrs(Attrs::decode(reader)?),
        FXP_EXTENDED_REPLY => Reply::Extended(reader.0.to_vec()),
        other => return Err(Error::Protocol(format!("unexpected packet type {other}"))),
    })
}

fn unexpected(reply: Reply) -> Error {
    let kind = match reply {
        Reply::Status { code, message } => return Error::Status { code, message },
        Reply::Handle(_) => "HANDLE",
        Reply::Data(_) => "DATA",
        Reply::Name(_) => "NAME",
        Reply::Attrs(_) => "ATTRS",
        Reply::Extended(_) => "EXTENDED_REPLY",
    };
    Error::Protocol(format!("unexpected {kind} reply"))
}

fn expect_ok(reply: Reply) -> Result<()> {
    match reply {
        Reply::Status { code: FX_OK, .. } => Ok(()),
        other => Err(unexpected(other)),
    }
}

fn expect_attrs(reply: Reply) -> Result<Attrs> {
    match reply {
        Reply::Attrs(attrs) => Ok(attrs),
        other => Err(unexpected(other)),
    }
}

fn expect_handle(reply: Reply) -> Result<Vec<u8>> {
    match reply {
        Reply::Handle(handle) => Ok(handle),
        other => Err(unexpected(other)),
    }
}

// ----------------------------------------------------------------- клиент

struct Pending {
    waiters: HashMap<u32, oneshot::Sender<Result<Reply>>>,
    /// Соединение закрыто: новых ответов не будет.
    closed: bool,
    /// Почему закрыто.
    reason: Option<String>,
    /// Когда сервер последний раз отвечал (или появился первый ждущий
    /// после простоя): по этому сторож замечает зависший сервер.
    last_activity: Instant,
}

/// Общее для задач чтения, записи и сторожа состояние соединения.
struct Link {
    pending: Mutex<Pending>,
    /// Становится `true`, когда соединение закрыто.
    closed: watch::Sender<bool>,
    inactivity: Duration,
}

impl Link {
    fn new(inactivity: Duration) -> Self {
        Link {
            pending: Mutex::new(Pending {
                waiters: HashMap::new(),
                closed: false,
                reason: None,
                last_activity: Instant::now(),
            }),
            closed: watch::Sender::new(false),
            inactivity,
        }
    }

    /// Закрывает соединение: все ждущие получают «соединение закрыто».
    fn shut(&self, reason: String) {
        {
            let mut pending = lock(&self.pending);
            if !pending.closed {
                pending.closed = true;
                pending.reason = Some(reason);
            }
            pending.waiters.clear();
        }
        self.closed.send_replace(true);
    }
}

struct Shared {
    out: mpsc::UnboundedSender<Vec<u8>>,
    link: Arc<Link>,
    next_id: AtomicU32,
    read_chunk: AtomicU32,
    write_chunk: AtomicU32,
}

/// Ручка SFTP-сессии; дешёво клонируется.
#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn io_error(err: std::io::Error) -> Error {
    if err.kind() == std::io::ErrorKind::UnexpectedEof {
        Error::Lost
    } else {
        Error::Protocol(err.to_string())
    }
}

async fn read_packet(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    reader.read_exact(&mut len).await.map_err(io_error)?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_PACKET {
        return Err(Error::Protocol(format!("bad packet length {len}")));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await.map_err(io_error)?;
    Ok(body)
}

/// Читает ответы и раздаёт их ждущим. При обрыве или нарушении протокола
/// закрывает соединение.
async fn read_loop(mut reader: impl AsyncRead + Unpin, link: Arc<Link>) {
    let reason = loop {
        let body = match read_packet(&mut reader).await {
            Ok(body) => body,
            Err(err) => break err.to_string(),
        };
        let mut packet = Reader(&body);
        let (Ok(kind), Ok(id)) = (packet.u8(), packet.u32()) else {
            break "packet too short".to_string();
        };
        let waiter = {
            let mut pending = lock(&link.pending);
            pending.last_activity = Instant::now();
            pending.waiters.remove(&id)
        };
        // Разбираем ответ, только если его ждут: ответы на брошенные
        // запросы просто выбрасываются.
        if let Some(waiter) = waiter {
            let _ = waiter.send(parse_reply(kind, &mut packet));
        }
    };
    link.shut(reason);
}

/// Пишет пакеты в ssh, собирая готовые в одну запись.
async fn write_loop(
    mut writer: impl AsyncWrite + Unpin,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    link: Arc<Link>,
) {
    const BATCH: usize = 512 * 1024;
    while let Some(mut buf) = rx.recv().await {
        while buf.len() < BATCH {
            match rx.try_recv() {
                Ok(more) => buf.extend_from_slice(&more),
                Err(_) => break,
            }
        }
        if writer.write_all(&buf).await.is_err() || writer.flush().await.is_err() {
            link.shut("could not write to ssh".to_string());
            break;
        }
    }
}

/// Сторож: если на запросы долго нет ни одного ответа, сервер завис —
/// закрываем соединение, и вкладка переподключится.
async fn watchdog(link: Arc<Link>) {
    let mut closed = link.closed.subscribe();
    let tick = (link.inactivity / 4).min(WATCHDOG_TICK);
    loop {
        tokio::select! {
            _ = closed.wait_for(|closed| *closed) => return,
            _ = tokio::time::sleep(tick) => {}
        }
        let stalled = {
            let pending = lock(&link.pending);
            !pending.waiters.is_empty() && pending.last_activity.elapsed() >= link.inactivity
        };
        if stalled {
            link.shut("the server stopped responding".to_string());
            return;
        }
    }
}

impl Client {
    /// Здоровается с сервером и запускает задачи чтения и записи в текущем
    /// tokio-рантайме.
    pub async fn start<R, W>(reader: R, writer: W) -> Result<Client>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::start_with(reader, writer, INACTIVITY_TIMEOUT).await
    }

    async fn start_with<R, W>(mut reader: R, mut writer: W, inactivity: Duration) -> Result<Client>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut init = Packet::new(FXP_INIT);
        init.u32(3);
        writer.write_all(&init.finish()).await.map_err(io_error)?;
        writer.flush().await.map_err(io_error)?;

        let body = read_packet(&mut reader).await?;
        let mut version = Reader(&body);
        if version.u8()? != FXP_VERSION {
            return Err(Error::Protocol("no version reply".to_string()));
        }
        let server_version = version.u32()?;
        if server_version < 3 {
            return Err(Error::Protocol(format!(
                "SFTP version {server_version} is not supported"
            )));
        }
        let mut extensions = Vec::new();
        while !version.0.is_empty() {
            let name = version.bytes()?.to_vec();
            version.bytes()?;
            extensions.push(name);
        }

        let (out, rx) = mpsc::unbounded_channel();
        let link = Arc::new(Link::new(inactivity));
        tokio::spawn(write_loop(writer, rx, Arc::clone(&link)));
        tokio::spawn(read_loop(reader, Arc::clone(&link)));
        tokio::spawn(watchdog(Arc::clone(&link)));
        let client = Client {
            shared: Arc::new(Shared {
                out,
                link,
                next_id: AtomicU32::new(1),
                read_chunk: AtomicU32::new(SAFE_CHUNK),
                write_chunk: AtomicU32::new(SAFE_CHUNK),
            }),
        };
        if extensions.iter().any(|name| name == b"limits@openssh.com") {
            // Без ответа остаёмся на безопасных 32 КиБ.
            if let Ok((read, write)) = client.limits().await {
                client.shared.read_chunk.store(chunk_size(read), Ordering::Relaxed);
                client.shared.write_chunk.store(chunk_size(write), Ordering::Relaxed);
            }
        }
        Ok(client)
    }

    /// Отправляет запрос сразу и возвращает ожидание ответа.
    fn call(&self, kind: u8, fill: impl FnOnce(&mut Packet)) -> BoxFuture<'static, Result<Reply>> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let mut packet = Packet::new(kind);
        packet.u32(id);
        fill(&mut packet);
        let (tx, rx) = oneshot::channel();
        let registered = {
            let mut pending = lock(&self.shared.link.pending);
            if !pending.closed {
                // После простоя отсчёт молчания сервера начинается заново.
                if pending.waiters.is_empty() {
                    pending.last_activity = Instant::now();
                }
                pending.waiters.insert(id, tx);
            }
            !pending.closed
        };
        let sent = registered && self.shared.out.send(packet.finish()).is_ok();
        if registered && !sent {
            lock(&self.shared.link.pending).waiters.remove(&id);
        }
        Box::pin(async move {
            if !sent {
                return Err(Error::Lost);
            }
            rx.await.map_err(|_| Error::Lost)?
        })
    }

    /// Отправляет запрос, не дожидаясь ответа (закрытие брошенной ручки).
    fn send_only(&self, kind: u8, fill: impl FnOnce(&mut Packet)) {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let mut packet = Packet::new(kind);
        packet.u32(id);
        fill(&mut packet);
        let _ = self.shared.out.send(packet.finish());
    }

    /// Завершается, когда соединение закрыто (обрыв, нарушение протокола,
    /// зависший сервер).
    pub fn closed(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut closed = self.shared.link.closed.subscribe();
        async move {
            let _ = closed.wait_for(|closed| *closed).await;
        }
    }

    /// Почему соединение закрыто (если закрыто).
    pub fn close_reason(&self) -> Option<String> {
        lock(&self.shared.link.pending).reason.clone()
    }

    async fn limits(&self) -> Result<(u64, u64)> {
        let reply = self
            .call(FXP_EXTENDED, |p| p.bytes(b"limits@openssh.com"))
            .await?;
        let Reply::Extended(data) = reply else {
            return Err(unexpected(reply));
        };
        let mut reader = Reader(&data);
        let _max_packet = reader.u64()?;
        Ok((reader.u64()?, reader.u64()?))
    }

    /// Размер куска для чтения и записи и сколько кусков держать в полёте.
    pub fn read_chunk(&self) -> u32 {
        self.shared.read_chunk.load(Ordering::Relaxed)
    }

    pub fn write_chunk(&self) -> u32 {
        self.shared.write_chunk.load(Ordering::Relaxed)
    }

    pub fn pipeline_depth(chunk: u32) -> usize {
        (IN_FLIGHT_BYTES / chunk.max(1)).clamp(4, 64) as usize
    }

    /// Абсолютный путь без `.`, `..` и ссылок (SFTP `realpath`).
    pub async fn realpath(&self, path: &Path) -> Result<PathBuf> {
        match self.call(FXP_REALPATH, |p| p.path(path)).await? {
            Reply::Name(mut names) if !names.is_empty() => {
                Ok(PathBuf::from(names.swap_remove(0).0))
            }
            other => Err(unexpected(other)),
        }
    }

    /// Атрибуты с переходом по ссылкам.
    pub async fn stat(&self, path: &Path) -> Result<Attrs> {
        expect_attrs(self.call(FXP_STAT, |p| p.path(path)).await?)
    }

    /// Атрибуты самой ссылки, без перехода.
    pub async fn lstat(&self, path: &Path) -> Result<Attrs> {
        expect_attrs(self.call(FXP_LSTAT, |p| p.path(path)).await?)
    }

    pub async fn setstat(&self, path: &Path, attrs: &Attrs) -> Result<()> {
        expect_ok(
            self.call(FXP_SETSTAT, |p| {
                p.path(path);
                attrs.encode(p);
            })
            .await?,
        )
    }

    pub async fn mkdir(&self, path: &Path) -> Result<()> {
        expect_ok(
            self.call(FXP_MKDIR, |p| {
                p.path(path);
                Attrs::default().encode(p);
            })
            .await?,
        )
    }

    pub async fn rmdir(&self, path: &Path) -> Result<()> {
        expect_ok(self.call(FXP_RMDIR, |p| p.path(path)).await?)
    }

    pub async fn remove(&self, path: &Path) -> Result<()> {
        expect_ok(self.call(FXP_REMOVE, |p| p.path(path)).await?)
    }

    /// Обычный SFTP `rename`: OpenSSH не заменяет существующий файл.
    pub async fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        expect_ok(
            self.call(FXP_RENAME, |p| {
                p.path(from);
                p.path(to);
            })
            .await?,
        )
    }

    /// Содержимое каталога без `.`, `..` и недопустимых имён (см.
    /// [`valid_name`]).
    pub async fn read_dir(&self, path: &Path) -> Result<Vec<(OsString, Attrs)>> {
        let handle = Handle {
            client: self.clone(),
            handle: expect_handle(self.call(FXP_OPENDIR, |p| p.path(path)).await?)?,
            open: true,
        };
        let mut entries = Vec::new();
        let mut received = 0usize;
        let mut empty_replies = 0u32;
        loop {
            match self.call(FXP_READDIR, |p| p.bytes(&handle.handle)).await? {
                Reply::Name(names) => {
                    // Сервер обязан когда-нибудь ответить EOF; бесконечные
                    // и пустые ответы обрываем.
                    if names.is_empty() {
                        empty_replies += 1;
                        if empty_replies > MAX_EMPTY_NAME_REPLIES {
                            return Err(Error::Protocol("endless directory listing".to_string()));
                        }
                    } else {
                        empty_replies = 0;
                    }
                    received += names.len();
                    if received > MAX_DIR_ENTRIES {
                        return Err(Error::Protocol("too many directory entries".to_string()));
                    }
                    entries.extend(names.into_iter().filter(|(name, _)| valid_name(name.as_bytes())));
                }
                Reply::Status { code: FX_EOF, .. } => break,
                other => return Err(unexpected(other)),
            }
        }
        handle.close().await?;
        Ok(entries)
    }

    /// Открывает файл; `attrs` применяются, если файл создаётся.
    pub async fn open(&self, path: &Path, flags: u32, attrs: &Attrs) -> Result<Handle> {
        let reply = self
            .call(FXP_OPEN, |p| {
                p.path(path);
                p.u32(flags);
                attrs.encode(p);
            })
            .await?;
        Ok(Handle {
            client: self.clone(),
            handle: expect_handle(reply)?,
            open: true,
        })
    }
}

fn chunk_size(limit: u64) -> u32 {
    if limit == 0 {
        SAFE_CHUNK
    } else {
        u32::try_from(limit).unwrap_or(u32::MAX).clamp(SAFE_CHUNK, MAX_CHUNK)
    }
}

/// Открытый файл или каталог. Если ручку бросили, не закрыв (передачу
/// отменили), закрытие уходит серверу без ожидания ответа.
pub struct Handle {
    client: Client,
    handle: Vec<u8>,
    open: bool,
}

impl Handle {
    /// Запрос куска; `None` — конец файла. Запрос уходит сразу, ответ
    /// можно ждать позже.
    pub fn read(&self, offset: u64, len: u32) -> BoxFuture<'static, Result<Option<Vec<u8>>>> {
        let reply = self.client.call(FXP_READ, |p| {
            p.bytes(&self.handle);
            p.u64(offset);
            p.u32(len);
        });
        Box::pin(async move {
            match reply.await? {
                // Пустой кусок или больше запрошенного — не по протоколу:
                // так сервер мог бы бесконечно «дописывать» файл.
                Reply::Data(data) if data.is_empty() => {
                    Err(Error::Protocol("empty DATA reply".to_string()))
                }
                Reply::Data(data) if data.len() > len as usize => {
                    Err(Error::Protocol("more data than requested".to_string()))
                }
                Reply::Data(data) => Ok(Some(data)),
                Reply::Status { code: FX_EOF, .. } => Ok(None),
                other => Err(unexpected(other)),
            }
        })
    }

    /// Запись куска; запрос уходит сразу.
    pub fn write(&self, offset: u64, data: &[u8]) -> BoxFuture<'static, Result<()>> {
        let reply = self.client.call(FXP_WRITE, |p| {
            p.bytes(&self.handle);
            p.u64(offset);
            p.bytes(data);
        });
        Box::pin(async move { expect_ok(reply.await?) })
    }

    pub async fn fsetstat(&self, attrs: &Attrs) -> Result<()> {
        expect_ok(
            self.client
                .call(FXP_FSETSTAT, |p| {
                    p.bytes(&self.handle);
                    attrs.encode(p);
                })
                .await?,
        )
    }

    pub async fn close(mut self) -> Result<()> {
        self.open = false;
        let handle = std::mem::take(&mut self.handle);
        expect_ok(self.client.call(FXP_CLOSE, |p| p.bytes(&handle)).await?)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.open {
            let handle = std::mem::take(&mut self.handle);
            self.client.send_only(FXP_CLOSE, |p| p.bytes(&handle));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attrs_round_trip() {
        let attrs = Attrs {
            size: Some(1 << 40),
            uid_gid: Some((1000, 100)),
            permissions: Some(0o100_644),
            times: Some((1_700_000_000, 1_700_000_100)),
        };
        let mut packet = Packet::new(0);
        attrs.encode(&mut packet);
        let bytes = packet.finish();
        let mut reader = Reader(&bytes[5..]);
        assert_eq!(Attrs::decode(&mut reader).unwrap(), attrs);
        assert!(reader.0.is_empty());
        assert!(attrs.is_file() && !attrs.is_dir() && !attrs.is_symlink());
        assert_eq!(attrs.modified(), Some(1_700_000_100));

        let empty = Attrs::default();
        let mut packet = Packet::new(0);
        empty.encode(&mut packet);
        assert_eq!(&packet.finish()[5..], &[0, 0, 0, 0]);
    }

    #[test]
    fn name_reply_keeps_raw_bytes() {
        // Ответ NAME с одним именем, которое не является UTF-8.
        let mut packet = Packet::new(FXP_NAME);
        packet.u32(1);
        packet.bytes(b"\xff\xfe.txt");
        packet.bytes(b"-rw-r--r-- 1 u u 0 Jan 1 00:00 ??.txt");
        Attrs {
            permissions: Some(0o100_600),
            ..Attrs::default()
        }
        .encode(&mut packet);
        let bytes = packet.finish();
        let mut reader = Reader(&bytes[5..]);
        match parse_reply(FXP_NAME, &mut reader).unwrap() {
            Reply::Name(names) => {
                assert_eq!(names.len(), 1);
                assert_eq!(names[0].0.as_bytes(), b"\xff\xfe.txt");
                assert!(names[0].1.is_file());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn status_without_message_and_truncated_packets() {
        let mut reader = Reader(&[0, 0, 0, 1]);
        match parse_reply(FXP_STATUS, &mut reader).unwrap() {
            Reply::Status { code, message } => {
                assert_eq!(code, FX_EOF);
                assert!(message.is_empty());
            }
            other => panic!("{other:?}"),
        }
        let mut reader = Reader(&[0, 0, 0, 9, b'a']);
        assert!(matches!(
            parse_reply(FXP_HANDLE, &mut reader),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn chunk_limits() {
        assert_eq!(chunk_size(0), SAFE_CHUNK);
        assert_eq!(chunk_size(1024), SAFE_CHUNK);
        assert_eq!(chunk_size(261_120), MAX_CHUNK);
        assert_eq!(chunk_size(100_000), 100_000);
        assert_eq!(Client::pipeline_depth(SAFE_CHUNK), 64);
        assert_eq!(Client::pipeline_depth(MAX_CHUNK), 8);
    }

    /// Полный обмен с поддельным сервером через пару каналов в памяти.
    #[tokio::test]
    async fn request_reply_over_pipe() {
        let (client_side, mut server_side) = tokio::io::duplex(1 << 20);
        let (client_read, client_write) = tokio::io::split(client_side);
        let server = tokio::spawn(async move {
            let init = read_packet(&mut server_side).await.unwrap();
            assert_eq!(init[0], FXP_INIT);
            let mut version = Packet::new(FXP_VERSION);
            version.u32(3);
            server_side.write_all(&version.finish()).await.unwrap();
            // realpath: отвечаем путём, который не является UTF-8.
            let request = read_packet(&mut server_side).await.unwrap();
            let mut reader = Reader(&request);
            assert_eq!(reader.u8().unwrap(), FXP_REALPATH);
            let id = reader.u32().unwrap();
            assert_eq!(reader.bytes().unwrap(), b"/home/\xe9t\xe9");
            let mut reply = Packet::new(FXP_NAME);
            reply.u32(id);
            reply.u32(1);
            reply.bytes(b"/home/\xe9t\xe9");
            reply.bytes(b"");
            Attrs::default().encode(&mut reply);
            server_side.write_all(&reply.finish()).await.unwrap();
        });
        let client = Client::start(client_read, client_write).await.unwrap();
        let path = PathBuf::from(OsString::from_vec(b"/home/\xe9t\xe9".to_vec()));
        assert_eq!(client.realpath(&path).await.unwrap(), path);
        server.await.unwrap();
        // Сервер ушёл: следующий запрос получает «соединение закрыто».
        assert_eq!(client.stat(&path).await, Err(Error::Lost));
    }

    /// Поддельный сервер: на каждый запрос `respond(тип, номер, тело)`
    /// возвращает пакеты ответа (пустой список — молчать).
    async fn fake_server(
        version: u32,
        inactivity: Duration,
        mut respond: impl FnMut(u8, u32, &[u8]) -> Vec<Vec<u8>> + Send + 'static,
    ) -> Result<Client> {
        let (client_side, mut server_side) = tokio::io::duplex(1 << 20);
        let (client_read, client_write) = tokio::io::split(client_side);
        tokio::spawn(async move {
            let Ok(init) = read_packet(&mut server_side).await else { return };
            assert_eq!(init[0], FXP_INIT);
            let mut hello = Packet::new(FXP_VERSION);
            hello.u32(version);
            if server_side.write_all(&hello.finish()).await.is_err() {
                return;
            }
            while let Ok(request) = read_packet(&mut server_side).await {
                let mut reader = Reader(&request);
                let kind = reader.u8().unwrap();
                let id = reader.u32().unwrap();
                for reply in respond(kind, id, reader.0) {
                    if server_side.write_all(&reply).await.is_err() {
                        return;
                    }
                }
            }
        });
        Client::start_with(client_read, client_write, inactivity).await
    }

    fn reply(kind: u8, id: u32, fill: impl FnOnce(&mut Packet)) -> Vec<u8> {
        let mut packet = Packet::new(kind);
        packet.u32(id);
        fill(&mut packet);
        packet.finish()
    }

    fn status(id: u32, code: u32, message: &str) -> Vec<u8> {
        reply(FXP_STATUS, id, |p| {
            p.u32(code);
            p.bytes(message.as_bytes());
            p.bytes(b"en");
        })
    }

    fn name_reply(id: u32, names: &[&[u8]]) -> Vec<u8> {
        reply(FXP_NAME, id, |p| {
            p.u32(names.len() as u32);
            for name in names {
                p.bytes(name);
                p.bytes(b"");
                Attrs::default().encode(p);
            }
        })
    }

    const SLOW: Duration = Duration::from_secs(60);

    #[test]
    fn name_validation() {
        for bad in [&b""[..], b".", b"..", b"/etc/x", b"../x", b"a/b", b"a\0b", b"/"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        for good in [&b"ok"[..], b".hidden", b"...", b"\xff\xfe", b"a b"] {
            assert!(valid_name(good), "{good:?}");
        }
    }

    #[test]
    fn server_messages_are_cleaned() {
        assert_eq!(sanitize_message("no\nsuch\x1b[31m file\0"), "no such [31m file");
        let long = sanitize_message(&"x".repeat(10_000));
        assert_eq!(long.chars().count(), MAX_MESSAGE_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    /// Враждебные имена в ответе READDIR выбрасываются.
    #[tokio::test]
    async fn read_dir_drops_hostile_names() {
        let mut sent = false;
        let client = fake_server(3, SLOW, move |kind, id, _| match kind {
            FXP_OPENDIR => vec![reply(FXP_HANDLE, id, |p| p.bytes(b"h"))],
            FXP_READDIR if !sent => {
                sent = true;
                vec![name_reply(
                    id,
                    &[b".", b"..", b"/etc/x", b"../x", b"a/b", b"", b"a\0b", b"ok", b"\xff"],
                )]
            }
            FXP_READDIR => vec![status(id, FX_EOF, "")],
            FXP_CLOSE => vec![status(id, FX_OK, "")],
            _ => vec![status(id, FX_OP_UNSUPPORTED, "")],
        })
        .await
        .unwrap();
        let names: Vec<Vec<u8>> = client
            .read_dir(Path::new("/d"))
            .await
            .unwrap()
            .into_iter()
            .map(|(name, _)| name.into_vec())
            .collect();
        assert_eq!(names, vec![b"ok".to_vec(), b"\xff".to_vec()]);
    }

    /// Бесконечный листинг (пустые ответы NAME без EOF) обрывается.
    #[tokio::test]
    async fn endless_listing_is_rejected() {
        let client = fake_server(3, SLOW, |kind, id, _| match kind {
            FXP_OPENDIR => vec![reply(FXP_HANDLE, id, |p| p.bytes(b"h"))],
            FXP_READDIR => vec![name_reply(id, &[])],
            _ => vec![status(id, FX_OK, "")],
        })
        .await
        .unwrap();
        assert!(matches!(
            client.read_dir(Path::new("/d")).await,
            Err(Error::Protocol(_))
        ));
    }

    /// Кусок больше запрошенного и пустой кусок — ошибка, а не данные.
    #[tokio::test]
    async fn oversized_and_empty_data_are_rejected() {
        let client = fake_server(3, SLOW, |kind, id, body| match kind {
            FXP_OPEN => vec![reply(FXP_HANDLE, id, |p| p.bytes(b"h"))],
            FXP_READ => {
                let mut reader = Reader(body);
                reader.bytes().unwrap();
                let offset = reader.u64().unwrap();
                let data = if offset == 0 { vec![7u8; 4096] } else { Vec::new() };
                vec![reply(FXP_DATA, id, |p| p.bytes(&data))]
            }
            _ => vec![status(id, FX_OK, "")],
        })
        .await
        .unwrap();
        let file = client.open(Path::new("/f"), OPEN_READ, &Attrs::default()).await.unwrap();
        assert!(matches!(file.read(0, 1024).await, Err(Error::Protocol(_))));
        assert_eq!(file.read(0, 4096).await.unwrap().map(|d| d.len()), Some(4096));
        assert!(matches!(file.read(4096, 1024).await, Err(Error::Protocol(_))));
    }

    #[tokio::test]
    async fn old_protocol_version_is_rejected() {
        let result = fake_server(2, SLOW, |_, id, _| vec![status(id, FX_OK, "")]).await;
        assert!(matches!(result, Err(Error::Protocol(_))));
    }

    /// Сервер молчит: сторож закрывает соединение, ждущие получают ошибку,
    /// `closed()` срабатывает, причина известна.
    #[tokio::test]
    async fn silent_server_is_dropped() {
        let client = fake_server(3, Duration::from_millis(300), |_, _, _| Vec::new())
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), client.stat(Path::new("/x"))).await;
        assert_eq!(result.expect("сторож должен сработать"), Err(Error::Lost));
        tokio::time::timeout(Duration::from_secs(1), client.closed())
            .await
            .expect("closed()");
        assert_eq!(client.close_reason().as_deref(), Some("the server stopped responding"));
    }

    /// Простой без запросов — не повод закрывать соединение.
    #[tokio::test]
    async fn idle_connection_stays_open() {
        let client = fake_server(3, Duration::from_millis(200), |kind, id, _| match kind {
            FXP_STAT => vec![reply(FXP_ATTRS, id, |p| Attrs::default().encode(p))],
            _ => Vec::new(),
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(client.stat(Path::new("/x")).await, Ok(Attrs::default()));
        assert_eq!(client.close_reason(), None);
    }

    /// Нарушение протокола закрывает соединение целиком.
    #[tokio::test]
    async fn protocol_violation_closes_connection() {
        let client = fake_server(3, SLOW, |_, _, _| vec![vec![0xff, 0xff, 0xff, 0xff]])
            .await
            .unwrap();
        assert_eq!(client.stat(Path::new("/x")).await, Err(Error::Lost));
        tokio::time::timeout(Duration::from_secs(1), client.closed())
            .await
            .expect("closed()");
        assert!(client.close_reason().unwrap().contains("bad packet length"));
    }
}
