//! Передача файлов и каталогов с прогрессом и отменой.
//!
//! Каждая передача сначала составляет план (обходит дерево и считает общий
//! объём), затем копирует файлы по одному. Отмена проверяется на каждой
//! сетевой операции. Ссылки на каталоги обходятся как обычные каталоги,
//! но не по кругу и не повторно (см. [`LinkGuard`]). Время изменения
//! переносится в обе стороны, новые файлы на сервере получают права
//! исходных.
//!
//! Сервер считается недоверенным: имена с него превращаются в пути только
//! через [`local_child`]/[`remote_child`], глубина и размер обходимого
//! дерева ограничены, скачивание не пишет заметно больше заявленного
//! размера, а временный файл создаётся под случайным именем и только новым.

use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::hash::{BuildHasher, RandomState};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use async_channel::Sender;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::proto::{self, Attrs, Client};
use super::{
    EntryKind, Event, MAX_TREE_DEPTH, MAX_TREE_ENTRIES, RemoteFile, RemoteStamp, SaveMode,
    TransferId, base_name, display, emit, error_text, join_remote, list_dir, local_child,
    parent_remote, remote_child,
};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Глубже этого по ссылкам не заходим даже без циклов: страховка от
/// длинных цепочек ссылок.
const MAX_DEPTH: usize = 64;
/// Сколько имён вида «имя (local copy N)» перебирать в поисках свободного.
const MAX_COPIES: u32 = 999;
/// Права, которые переносятся на сервер: без setuid, setgid и sticky.
const MODE_MASK: u32 = 0o777;
/// Насколько скачанный файл может перерасти заявленный размер (файл
/// дописывают во время скачивания): не больше ещё одного такого же размера
/// и не меньше этого запаса. Дальше скачивание останавливается.
const DOWNLOAD_SLACK: u64 = 64 * 1024 * 1024;
/// Попыток подобрать свободное имя временного файла.
const PART_ATTEMPTS: u32 = 8;

/// Причина досрочного завершения передачи.
pub(super) enum Abort {
    Cancelled,
    /// Файл на сервере изменился после скачивания (`None` — удалён).
    Conflict(Option<RemoteStamp>),
    Failed(String),
}

type Step<T> = Result<T, Abort>;

/// Состояние одной передачи.
pub(super) struct Job {
    sftp: Client,
    events: Sender<Event>,
    id: TransferId,
    cancel: CancellationToken,
    done: u64,
    total: u64,
    last_emit: Option<Instant>,
    /// Пути, которые не передавались (спецфайлы, битые и зацикленные ссылки).
    pub(super) skipped: Vec<PathBuf>,
    /// Скачанные файлы, размер которых не совпал с заявленным сервером.
    pub(super) size_mismatch: Vec<PathBuf>,
    /// Итог одиночной передачи файла.
    pub(super) file: Option<RemoteFile>,
}

impl Job {
    pub(super) fn new(
        sftp: Client,
        events: Sender<Event>,
        id: TransferId,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            sftp,
            events,
            id,
            cancel,
            done: 0,
            total: 0,
            last_emit: None,
            skipped: Vec::new(),
            size_mismatch: Vec::new(),
            file: None,
        }
    }

    fn progress(&mut self, current: &str, force: bool) {
        let now = Instant::now();
        let due = self
            .last_emit
            .is_none_or(|last| now.duration_since(last) >= PROGRESS_INTERVAL);
        if force || due {
            self.last_emit = Some(now);
            emit(
                &self.events,
                Event::Progress {
                    id: self.id,
                    done: self.done,
                    total: self.total.max(self.done),
                    current: current.to_string(),
                },
            );
        }
    }
}

/// Выполняет операцию, прерывая её при отмене передачи.
async fn guard<T, E>(
    cancel: &CancellationToken,
    context: &str,
    operation: impl Future<Output = Result<T, E>>,
) -> Step<T>
where
    E: std::error::Error + 'static,
{
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Abort::Cancelled),
        result = operation => {
            result.map_err(|err| Abort::Failed(format!("{context}: {}", error_text(&err))))
        }
    }
}

/// Как [`guard`], но ошибку операции оставляет вызывающему.
async fn cancellable<T>(cancel: &CancellationToken, operation: impl Future<Output = T>) -> Step<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Abort::Cancelled),
        result = operation => Ok(result),
    }
}

fn name_of(path: &Path) -> String {
    display(Path::new(base_name(path)))
}

/// Секунды Unix для SFTP (32 бита); время до 1970 года или после 2106-го
/// не переносится.
fn unix_secs(secs: i64) -> Option<u32> {
    u32::try_from(secs).ok()
}

// ----------------------------------------------------------- ссылки

/// Решает, заходить ли по ссылке на каталог.
///
/// `chain` — настоящие (без ссылок) пути каталогов от корня передачи до
/// текущего. Нельзя заходить, если цель содержит один из них (так
/// получился бы бесконечный цикл), если по этой цели уже заходили по
/// другой ссылке (повтор одного и того же дерева может размножиться
/// лавиной) и если цепочка уже слишком длинная.
#[derive(Default)]
struct LinkGuard {
    followed: HashSet<PathBuf>,
}

impl LinkGuard {
    fn admit(&mut self, chain: &[PathBuf], target: &Path) -> bool {
        if chain.len() >= MAX_DEPTH {
            return false;
        }
        if chain.iter().any(|dir| dir.starts_with(target)) {
            return false;
        }
        self.followed.insert(target.to_path_buf())
    }
}

/// Обход дерева не должен расти без конца: дерево на враждебном сервере
/// может быть бесконечным.
fn check_plan_size(len: usize) -> Step<()> {
    if len >= MAX_TREE_ENTRIES {
        return Err(Abort::Failed(tr!(
            "Too many items to transfer (more than {})",
            MAX_TREE_ENTRIES
        )));
    }
    Ok(())
}

fn scanning_text(found: usize) -> String {
    trn!("Scanning: {} item found", "Scanning: {} items found", found)
}

fn extend_chain(chain: &[PathBuf], next: PathBuf) -> Vec<PathBuf> {
    let mut chain = chain.to_vec();
    chain.push(next);
    chain
}

// ---------------------------------------------------------------- скачивание

enum DownloadItem {
    Dir {
        local: PathBuf,
        modified: Option<u64>,
    },
    File {
        remote: PathBuf,
        local: PathBuf,
        /// Размер по словам сервера.
        size: Option<u64>,
        modified: Option<u64>,
    },
}

pub(super) async fn download(job: &mut Job, sources: Vec<PathBuf>, dest_dir: PathBuf) -> Step<()> {
    let cancel = job.cancel.clone();
    let sftp = job.sftp.clone();
    let mut plan = Vec::new();
    let mut links = LinkGuard::default();

    for source in &sources {
        // Имя берётся из удалённого пути; «..», «/» и прочее не годятся.
        let Some(local) = local_child(&dest_dir, base_name(source)) else {
            job.skipped.push(source.clone());
            continue;
        };
        let context = display(source);
        // stat переходит по ссылкам: выбранная ссылка скачивается как то,
        // на что она указывает.
        let attrs = guard(&cancel, &context, sftp.stat(source)).await?;
        let modified = attrs.modified().map(u64::from);
        if attrs.is_dir() {
            let real = guard(&cancel, &context, sftp.realpath(source)).await?;
            links.followed.insert(real.clone());
            plan.push(DownloadItem::Dir {
                local: local.clone(),
                modified,
            });
            scan_remote(job, source, real, &local, &mut links, &mut plan).await?;
        } else if attrs.is_file() {
            if sources.len() == 1 {
                job.file = Some(RemoteFile {
                    path: source.clone(),
                    stamp: RemoteStamp::of(&attrs),
                });
            }
            plan.push(DownloadItem::File {
                remote: source.clone(),
                local,
                size: attrs.size,
                modified,
            });
        } else {
            job.skipped.push(source.clone());
        }
    }

    job.total = plan
        .iter()
        .map(|item| match item {
            DownloadItem::File { size, .. } => size.unwrap_or(0),
            DownloadItem::Dir { .. } => 0,
        })
        .fold(0u64, u64::saturating_add);
    job.progress("", true);

    let mut created = Vec::new();
    for item in plan {
        match item {
            DownloadItem::Dir { local, modified } => {
                let context = local.display().to_string();
                let existed = tokio::fs::try_exists(&local).await.unwrap_or(false);
                guard(&cancel, &context, tokio::fs::create_dir_all(&local)).await?;
                if !existed {
                    created.push((local, modified));
                }
            }
            DownloadItem::File {
                remote,
                local,
                size,
                modified,
            } => {
                download_file(job, &remote, &local, size, modified).await?;
            }
        }
    }
    // Время созданных каталогов ставится в конце, от глубоких к верхним:
    // запись файлов внутрь его меняет.
    for (dir, modified) in created.iter().rev() {
        if let Some(modified) = modified {
            let _ = set_local_mtime(dir, *modified);
        }
    }
    job.progress("", true);
    Ok(())
}

fn set_local_mtime(path: &Path, secs: u64) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

async fn scan_remote(
    job: &mut Job,
    remote_root: &Path,
    real_root: PathBuf,
    local_root: &Path,
    links: &mut LinkGuard,
    plan: &mut Vec<DownloadItem>,
) -> Step<()> {
    let cancel = job.cancel.clone();
    let sftp = job.sftp.clone();
    let mut pending = vec![(remote_root.to_path_buf(), local_root.to_path_buf(), vec![real_root])];
    while let Some((remote_dir, local_dir, chain)) = pending.pop() {
        let context = display(&remote_dir);
        let entries = guard(&cancel, &context, list_dir(&sftp, &remote_dir)).await?;
        let real_dir = chain.last().cloned().unwrap_or_else(|| remote_dir.clone());
        for entry in entries {
            check_plan_size(plan.len())?;
            // Пути строятся только из проверенных имён: и локальный, и
            // удалённый (имена уже отфильтрованы при чтении каталога).
            let (Some(remote), Some(local)) = (
                remote_child(&remote_dir, &entry.name),
                local_child(&local_dir, &entry.name),
            ) else {
                job.skipped.push(entry.path);
                continue;
            };
            match (entry.kind, entry.is_link) {
                (EntryKind::Dir, _) if chain.len() >= MAX_TREE_DEPTH => job.skipped.push(remote),
                (EntryKind::Dir, false) => {
                    // Обычный подкаталог: его настоящий путь — путь
                    // родителя плюс имя, спрашивать сервер не нужно.
                    let chain = extend_chain(&chain, join_remote(&real_dir, &entry.name));
                    plan.push(DownloadItem::Dir {
                        local: local.clone(),
                        modified: entry.modified,
                    });
                    pending.push((remote, local, chain));
                }
                (EntryKind::Dir, true) => {
                    // Куда ссылка ведёт на самом деле (SFTP realpath).
                    let target = match cancellable(&cancel, sftp.realpath(&remote)).await? {
                        Ok(target) if links.admit(&chain, &target) => target,
                        _ => {
                            job.skipped.push(remote);
                            continue;
                        }
                    };
                    plan.push(DownloadItem::Dir {
                        local: local.clone(),
                        modified: entry.modified,
                    });
                    pending.push((remote, local, extend_chain(&chain, target)));
                }
                (EntryKind::File, _) => plan.push(DownloadItem::File {
                    remote,
                    local,
                    size: entry.size,
                    modified: entry.modified,
                }),
                (EntryKind::Other, _) => job.skipped.push(remote),
            }
        }
        // Обход большого дерева виден и отменяем.
        job.progress(&scanning_text(plan.len()), false);
    }
    Ok(())
}

/// Скачивает файл во временный сосед и переименовывает по готовности, чтобы
/// прерванная передача не оставляла испорченный файл под настоящим именем.
async fn download_file(
    job: &mut Job,
    remote: &Path,
    local: &Path,
    size: Option<u64>,
    modified: Option<u64>,
) -> Step<()> {
    let dir = local.parent().unwrap_or_else(|| Path::new("."));
    let (part, writer) = create_part(&job.cancel, dir).await?;
    let result = async {
        copy_down(job, remote, writer, &part, size, modified).await?;
        let context = local.display().to_string();
        guard(&job.cancel, &context, tokio::fs::rename(&part, local)).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    result
}

/// Короткое случайное имя временного файла: не упирается в предел длины
/// имени и не совпадает с настоящими файлами.
fn part_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let random = RandomState::new().hash_one((nanos, std::process::id()));
    format!(".sshdeck-{random:016x}.part")
}

/// Создаёт временный файл в `dir` только новым (O_EXCL): существующий файл
/// с тем же именем никогда не перезаписывается.
async fn create_part(cancel: &CancellationToken, dir: &Path) -> Step<(PathBuf, tokio::fs::File)> {
    let mut last_error = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    for _ in 0..PART_ATTEMPTS {
        let part = dir.join(part_name());
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        match cancellable(cancel, options.open(&part)).await? {
            Ok(file) => return Ok((part, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => last_error = err,
            Err(err) => {
                return Err(Abort::Failed(format!("{}: {}", part.display(), error_text(&err))));
            }
        }
    }
    Err(Abort::Failed(format!("{}: {}", dir.display(), error_text(&last_error))))
}

/// Сколько байтов можно скачать при заявленном размере `size`.
fn download_limit(size: Option<u64>) -> u64 {
    let size = size.unwrap_or(0);
    size.saturating_add(size.max(DOWNLOAD_SLACK))
}

/// Конвейерное чтение: несколько запросов кусков держатся в полёте, ответы
/// обрабатываются по порядку смещений.
async fn copy_down(
    job: &mut Job,
    remote: &Path,
    mut writer: tokio::fs::File,
    part: &Path,
    size: Option<u64>,
    modified: Option<u64>,
) -> Step<()> {
    let cancel = job.cancel.clone();
    let name = name_of(remote);
    let context = display(remote);
    let part_context = part.display().to_string();

    let file = guard(
        &cancel,
        &context,
        job.sftp.open(remote, proto::OPEN_READ, &Attrs::default()),
    )
    .await?;

    let chunk = job.sftp.read_chunk();
    let depth = Client::pipeline_depth(chunk);
    // Дальше этого не запрашиваем: сервер не должен «дописывать» файл
    // бесконечно.
    let limit = download_limit(size);
    let mut queue = VecDeque::new();
    let mut next = 0u64;
    let mut written = 0u64;
    let mut at_end = false;
    loop {
        while !at_end && queue.len() < depth && next < limit {
            queue.push_back((next, file.read(next, chunk)));
            next = next.saturating_add(u64::from(chunk));
        }
        let Some((offset, reply)) = queue.pop_front() else {
            break;
        };
        match guard(&cancel, &context, reply).await? {
            None => {
                at_end = true;
                queue.clear();
            }
            Some(data) => {
                guard(&cancel, &part_context, writer.write_all(&data)).await?;
                written += data.len() as u64;
                job.done += data.len() as u64;
                job.progress(&name, false);
                // Пустых и слишком длинных ответов не бывает: их отсекает
                // клиент протокола.
                if data.len() < chunk as usize {
                    // Короткий ответ: близок конец файла или сервер режет
                    // куски. Следующие запросы рассчитаны на полные куски,
                    // поэтому продолжаем с фактического места заново.
                    queue.clear();
                    next = offset + data.len() as u64;
                }
            }
        }
    }
    guard(&cancel, &part_context, writer.flush()).await?;
    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, file.close()).await;
    // Размер не сошёлся: файл меняли во время скачивания, сервер оборвал
    // его раньше времени или упёрлись в предел — сообщим пользователю.
    if !at_end || size.is_some_and(|size| size != written) {
        job.size_mismatch.push(remote.to_path_buf());
    }
    if let Some(modified) = modified {
        let writer = writer.into_std().await;
        let _ = writer.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(modified));
    }
    Ok(())
}

// ------------------------------------------------------------------ загрузка

/// Права и время локального файла, которые переносятся на сервер.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalMeta {
    mode: u32,
    /// Время доступа и изменения.
    times: Option<(u32, u32)>,
}

impl LocalMeta {
    fn of(meta: &std::fs::Metadata) -> Self {
        let times = unix_secs(meta.atime()).zip(unix_secs(meta.mtime()));
        Self {
            mode: meta.mode() & MODE_MASK,
            times,
        }
    }
}

enum UploadItem {
    Dir {
        remote: PathBuf,
        meta: LocalMeta,
    },
    File {
        local: PathBuf,
        remote: PathBuf,
        size: u64,
        meta: LocalMeta,
    },
}

pub(super) async fn upload(job: &mut Job, sources: Vec<PathBuf>, dest_dir: PathBuf) -> Step<()> {
    let cancel = job.cancel.clone();
    let mut plan = Vec::new();
    let mut links = LinkGuard::default();

    for source in &sources {
        let context = source.display().to_string();
        let Some(name) = source.file_name() else {
            job.skipped.push(source.clone());
            continue;
        };
        let remote = join_remote(&dest_dir, name);
        let meta = guard(&cancel, &context, tokio::fs::metadata(source)).await?;
        if meta.is_dir() {
            let real = guard(&cancel, &context, tokio::fs::canonicalize(source)).await?;
            links.followed.insert(real.clone());
            plan.push(UploadItem::Dir {
                remote: remote.clone(),
                meta: LocalMeta::of(&meta),
            });
            scan_local(job, source, real, &remote, &mut links, &mut plan).await?;
        } else if meta.is_file() {
            plan.push(UploadItem::File {
                local: source.clone(),
                remote,
                size: meta.len(),
                meta: LocalMeta::of(&meta),
            });
        } else {
            job.skipped.push(source.clone());
        }
    }
    run_upload_plan(job, plan).await
}

/// Заливает один файл по точному удалённому пути (сохранение из редактора).
pub(super) async fn upload_file(
    job: &mut Job,
    local: PathBuf,
    remote: PathBuf,
    mode: SaveMode,
) -> Step<()> {
    let cancel = job.cancel.clone();
    let sftp = job.sftp.clone();
    let context = local.display().to_string();
    let meta = guard(&cancel, &context, tokio::fs::metadata(&local)).await?;

    let remote = match mode {
        SaveMode::IfUnchanged(expected) => {
            let current = match cancellable(&cancel, sftp.stat(&remote)).await? {
                Ok(attrs) => Some(RemoteStamp::of(&attrs)),
                Err(err) if err.is_not_found() => None,
                Err(err) => {
                    return Err(Abort::Failed(format!("{}: {}", display(&remote), error_text(&err))));
                }
            };
            if current != Some(expected) {
                return Err(Abort::Conflict(current));
            }
            remote
        }
        SaveMode::Overwrite => remote,
        SaveMode::Copy => free_copy_path(job, &remote).await?,
    };

    job.total = meta.len();
    job.progress("", true);
    upload_one(job, &local, &remote, LocalMeta::of(&meta), mode == SaveMode::Copy).await?;
    let attrs = guard(&cancel, &display(&remote), sftp.stat(&remote)).await?;
    job.file = Some(RemoteFile {
        path: remote,
        stamp: RemoteStamp::of(&attrs),
    });
    job.progress("", true);
    Ok(())
}

/// Имя копии рядом с файлом: «config (local copy).yaml», затем
/// «config (local copy 2).yaml» и т. д. Расширение остаётся в конце, чтобы
/// копия открывалась тем же редактором; у скрытых файлов без расширения
/// («.bashrc») суффикс просто дописывается.
pub fn copy_name(name: &OsStr, n: u32) -> OsString {
    let bytes = name.as_bytes();
    let dot = bytes
        .iter()
        .rposition(|&b| b == b'.')
        .filter(|&idx| idx > 0 && idx + 1 < bytes.len());
    let (stem, ext) = match dot {
        Some(idx) => bytes.split_at(idx),
        None => (bytes, &b""[..]),
    };
    let suffix = if n <= 1 {
        " (local copy)".to_string()
    } else {
        format!(" (local copy {n})")
    };
    let mut out = stem.to_vec();
    out.extend_from_slice(suffix.as_bytes());
    out.extend_from_slice(ext);
    OsString::from_vec(out)
}

async fn free_copy_path(job: &mut Job, remote: &Path) -> Step<PathBuf> {
    let dir = parent_remote(remote);
    let name = base_name(remote);
    for n in 1..=MAX_COPIES {
        let candidate = join_remote(&dir, copy_name(name, n));
        match cancellable(&job.cancel, job.sftp.lstat(&candidate)).await? {
            Ok(_) => continue,
            Err(err) if err.is_not_found() => return Ok(candidate),
            Err(err) => {
                return Err(Abort::Failed(format!("{}: {}", display(&candidate), error_text(&err))));
            }
        }
    }
    Err(Abort::Failed(tr!(
        "There is no free name for a copy of “{}”",
        name_of(remote)
    )))
}

async fn run_upload_plan(job: &mut Job, plan: Vec<UploadItem>) -> Step<()> {
    job.total = plan
        .iter()
        .map(|item| match item {
            UploadItem::File { size, .. } => *size,
            UploadItem::Dir { .. } => 0,
        })
        .sum();
    job.progress("", true);

    let mut created = Vec::new();
    for item in plan {
        match item {
            UploadItem::Dir { remote, meta } => {
                if ensure_remote_dir(job, &remote).await? {
                    created.push((remote, meta));
                }
            }
            UploadItem::File {
                local,
                remote,
                meta,
                ..
            } => upload_one(job, &local, &remote, meta, false).await?,
        }
    }
    // Права и время новых каталогов ставятся в конце, от глубоких к
    // верхним: каталог без права записи не дал бы залить содержимое, а
    // запись внутрь меняет время. Ошибки здесь не мешают передаче.
    let cancel = job.cancel.clone();
    for (remote, meta) in created.iter().rev() {
        let attrs = Attrs {
            permissions: Some(meta.mode),
            times: meta.times,
            ..Attrs::default()
        };
        let _ = cancellable(&cancel, job.sftp.setstat(remote, &attrs)).await?;
    }
    job.progress("", true);
    Ok(())
}

async fn scan_local(
    job: &mut Job,
    local_root: &Path,
    real_root: PathBuf,
    remote_root: &Path,
    links: &mut LinkGuard,
    plan: &mut Vec<UploadItem>,
) -> Step<()> {
    let cancel = job.cancel.clone();
    let mut pending = vec![(local_root.to_path_buf(), remote_root.to_path_buf(), vec![real_root])];
    while let Some((local_dir, remote_dir, chain)) = pending.pop() {
        let context = local_dir.display().to_string();
        let real_dir = chain.last().cloned().unwrap_or_else(|| local_dir.clone());
        let mut reader = guard(&cancel, &context, tokio::fs::read_dir(&local_dir)).await?;
        while let Some(entry) = guard(&cancel, &context, reader.next_entry()).await? {
            check_plan_size(plan.len())?;
            let path = entry.path();
            let name = entry.file_name();
            let entry_context = path.display().to_string();
            let remote = join_remote(&remote_dir, &name);
            let file_type = guard(&cancel, &entry_context, entry.file_type()).await?;
            let too_deep = chain.len() >= MAX_TREE_DEPTH;
            if too_deep && (file_type.is_dir() || file_type.is_symlink()) {
                job.skipped.push(path);
            } else if file_type.is_dir() {
                let meta = guard(&cancel, &entry_context, entry.metadata()).await?;
                plan.push(UploadItem::Dir {
                    remote: remote.clone(),
                    meta: LocalMeta::of(&meta),
                });
                pending.push((path, remote, extend_chain(&chain, real_dir.join(&name))));
            } else if file_type.is_file() {
                let meta = guard(&cancel, &entry_context, entry.metadata()).await?;
                plan.push(UploadItem::File {
                    local: path,
                    remote,
                    size: meta.len(),
                    meta: LocalMeta::of(&meta),
                });
            } else if file_type.is_symlink() {
                // Ссылка на файл загружается как файл, ссылка на каталог —
                // как каталог (если не ведёт по кругу). Битые ссылки и
                // ссылки на спецфайлы пропускаются.
                match tokio::fs::metadata(&path).await {
                    Ok(meta) if meta.is_file() => plan.push(UploadItem::File {
                        local: path,
                        remote,
                        size: meta.len(),
                        meta: LocalMeta::of(&meta),
                    }),
                    Ok(meta) if meta.is_dir() => match tokio::fs::canonicalize(&path).await {
                        Ok(target) if links.admit(&chain, &target) => {
                            plan.push(UploadItem::Dir {
                                remote: remote.clone(),
                                meta: LocalMeta::of(&meta),
                            });
                            pending.push((path, remote, extend_chain(&chain, target)));
                        }
                        _ => job.skipped.push(path),
                    },
                    _ => job.skipped.push(path),
                }
            } else {
                job.skipped.push(path);
            }
        }
        job.progress(&scanning_text(plan.len()), false);
    }
    Ok(())
}

/// Создаёт удалённый каталог; `true` — если он создан сейчас, `false` —
/// если уже был.
async fn ensure_remote_dir(job: &mut Job, remote: &Path) -> Step<bool> {
    let sftp = job.sftp.clone();
    guard(&job.cancel, &display(remote), async {
        match sftp.mkdir(remote).await {
            Ok(()) => Ok(true),
            // Каталог уже есть — это не ошибка.
            Err(err) => match sftp.stat(remote).await {
                Ok(attrs) if attrs.is_dir() => Ok(false),
                _ => Err(err),
            },
        }
    })
    .await
}

/// Пишет прямо в целевой файл (с усечением), а не через временный: так у
/// существующего файла сохраняются владелец и права. `exclusive` — файл
/// должен быть новым (копия рядом с оригиналом).
async fn upload_one(
    job: &mut Job,
    local: &Path,
    remote: &Path,
    meta: LocalMeta,
    exclusive: bool,
) -> Step<()> {
    let sftp = job.sftp.clone();
    // Существует ли файл: от этого зависит, можно ли убрать его при сбое.
    // Ошибка, отличная от «нет такого файла», — не повод считать его новым.
    let existed = if exclusive {
        false
    } else {
        match cancellable(&job.cancel, sftp.lstat(remote)).await? {
            Ok(_) => true,
            Err(err) if err.is_not_found() => false,
            Err(err) => {
                return Err(Abort::Failed(format!("{}: {}", display(remote), error_text(&err))));
            }
        }
    };
    let mut created = false;
    let result = copy_up(job, local, remote, meta, existed, exclusive, &mut created).await;
    if result.is_err() && created {
        // Недописанный новый файл убираем; если не вышло — не страшно.
        let cleanup = sftp.remove(remote);
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, cleanup).await;
    }
    result
}

/// Конвейерная запись: несколько кусков в полёте, подтверждения по порядку.
#[allow(clippy::too_many_arguments)]
async fn copy_up(
    job: &mut Job,
    local: &Path,
    remote: &Path,
    meta: LocalMeta,
    existed: bool,
    exclusive: bool,
    created: &mut bool,
) -> Step<()> {
    let cancel = job.cancel.clone();
    let name = name_of(remote);
    let context = display(remote);
    let local_context = local.display().to_string();

    let mut reader = guard(&cancel, &local_context, tokio::fs::File::open(local)).await?;
    let flags = proto::OPEN_WRITE
        | proto::OPEN_CREATE
        | if exclusive {
            proto::OPEN_EXCLUSIVE
        } else {
            proto::OPEN_TRUNCATE
        };
    // Новый файл сразу создаётся с правами исходного: umask сервера может
    // их только урезать, поэтому лишнего доступа не бывает даже на миг.
    let create_attrs = Attrs {
        permissions: Some(meta.mode),
        ..Attrs::default()
    };
    let file = guard(&cancel, &context, job.sftp.open(remote, flags, &create_attrs)).await?;
    if !existed {
        *created = true;
        // А теперь точно как у исходного, без поправки на umask.
        let _ = cancellable(&cancel, file.fsetstat(&create_attrs)).await?;
    }

    let chunk = job.sftp.write_chunk();
    let depth = Client::pipeline_depth(chunk);
    let mut buf = vec![0u8; chunk as usize];
    let mut queue = VecDeque::new();
    let mut offset = 0u64;
    loop {
        let n = guard(&cancel, &local_context, reader.read(&mut buf)).await?;
        if n == 0 {
            break;
        }
        queue.push_back((n, file.write(offset, &buf[..n])));
        offset += n as u64;
        if queue.len() >= depth
            && let Some((n, reply)) = queue.pop_front()
        {
            guard(&cancel, &context, reply).await?;
            job.done += n as u64;
            job.progress(&name, false);
        }
    }
    while let Some((n, reply)) = queue.pop_front() {
        guard(&cancel, &context, reply).await?;
        job.done += n as u64;
        job.progress(&name, false);
    }
    // Время изменения — как у исходного, и у перезаписанного файла тоже.
    // Чужой файл сервер может не дать пометить — это не ошибка.
    if let Some(times) = meta.times {
        let attrs = Attrs {
            times: Some(times),
            ..Attrs::default()
        };
        let _ = cancellable(&cancel, file.fsetstat(&attrs)).await?;
    }
    guard(&cancel, &context, file.close()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(bytes: &[u8]) -> OsString {
        OsString::from_vec(bytes.to_vec())
    }

    #[test]
    fn copy_names() {
        assert_eq!(copy_name(OsStr::new("config.yaml"), 1), "config (local copy).yaml");
        assert_eq!(copy_name(OsStr::new("config.yaml"), 2), "config (local copy 2).yaml");
        assert_eq!(copy_name(OsStr::new(".bashrc"), 1), ".bashrc (local copy)");
        assert_eq!(copy_name(OsStr::new("Makefile"), 3), "Makefile (local copy 3)");
        assert_eq!(copy_name(OsStr::new("a.tar.gz"), 1), "a.tar (local copy).gz");
        assert_eq!(copy_name(OsStr::new("x."), 1), "x. (local copy)");
        assert_eq!(copy_name(&os(b"\xff.txt"), 1), os(b"\xff (local copy).txt"));
    }

    #[test]
    fn link_guard_stops_cycles_and_repeats() {
        let chain = vec![PathBuf::from("/srv/app"), PathBuf::from("/srv/app/lib")];
        let mut guard = LinkGuard::default();
        // Ссылка на предка или на текущий каталог — цикл.
        assert!(!guard.admit(&chain, Path::new("/srv")));
        assert!(!guard.admit(&chain, Path::new("/srv/app")));
        assert!(!guard.admit(&chain, Path::new("/srv/app/lib")));
        assert!(!guard.admit(&chain, Path::new("/")));
        // Соседний каталог можно, но только один раз.
        assert!(guard.admit(&chain, Path::new("/srv/data")));
        assert!(!guard.admit(&chain, Path::new("/srv/data")));
        // Похожее имя — не предок.
        assert!(guard.admit(&chain, Path::new("/srv/ap")));
        // Слишком длинная цепочка.
        let deep: Vec<PathBuf> = (0..MAX_DEPTH).map(|i| PathBuf::from(format!("/d{i}"))).collect();
        assert!(!guard.admit(&deep, Path::new("/elsewhere")));
    }

    #[test]
    fn part_names_are_short_and_unique() {
        let a = part_name();
        let b = part_name();
        assert_ne!(a, b);
        assert_eq!(a.len(), ".sshdeck-0123456789abcdef.part".len());
        assert!(a.starts_with(".sshdeck-") && a.ends_with(".part"));
    }

    #[tokio::test]
    async fn part_file_never_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let (part, _file) = create_part(&cancel, dir.path()).await.ok().unwrap();
        assert_eq!(part.parent(), Some(dir.path()));
        assert!(part.exists());
    }

    #[test]
    fn download_limits() {
        assert_eq!(download_limit(Some(10)), 10 + DOWNLOAD_SLACK);
        assert_eq!(download_limit(None), DOWNLOAD_SLACK);
        assert_eq!(download_limit(Some(1 << 40)), 2 << 40);
        assert_eq!(download_limit(Some(u64::MAX)), u64::MAX);
    }

    #[test]
    fn local_meta_masks_special_bits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4755)).unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000)).unwrap();
        let meta = LocalMeta::of(&std::fs::metadata(&path).unwrap());
        assert_eq!(meta.mode, 0o755);
        assert_eq!(meta.times.map(|t| t.1), Some(1_600_000_000));
        assert_eq!(unix_secs(-1), None);
    }
}
