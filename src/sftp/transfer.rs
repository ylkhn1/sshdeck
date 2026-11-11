//! Передача файлов и каталогов с прогрессом и отменой.
//!
//! Каждая передача сначала составляет план (обходит дерево и считает общий
//! объём), затем копирует файлы по одному. Отмена проверяется на каждой
//! сетевой операции.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_channel::Sender;
use openssh_sftp_client::Sftp;
use openssh_sftp_client::file::TokioCompatFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::{EntryKind, Event, TransferId, base_name, emit, error_text, join_remote, list_dir};

const BUFFER_SIZE: usize = 128 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Причина досрочного завершения передачи.
pub(super) enum Abort {
    Cancelled,
    Failed(String),
}

type Step<T> = Result<T, Abort>;

/// Состояние одной передачи.
pub(super) struct Job {
    sftp: Arc<Sftp>,
    events: Sender<Event>,
    id: TransferId,
    cancel: CancellationToken,
    done: u64,
    total: u64,
    last_emit: Option<Instant>,
    /// Пути, которые не передавались (ссылки на каталоги, спецфайлы).
    pub(super) skipped: Vec<String>,
}

impl Job {
    pub(super) fn new(
        sftp: Arc<Sftp>,
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

// ---------------------------------------------------------------- скачивание

enum DownloadItem {
    Dir(PathBuf),
    File {
        remote: String,
        local: PathBuf,
        size: u64,
    },
}

pub(super) async fn download(job: &mut Job, sources: Vec<String>, dest_dir: PathBuf) -> Step<()> {
    let cancel = job.cancel.clone();
    let sftp = Arc::clone(&job.sftp);
    let mut plan = Vec::new();

    for source in &sources {
        let local = dest_dir.join(base_name(source));
        // metadata() переходит по ссылкам: выбранная ссылка скачивается как
        // то, на что она указывает.
        let meta = guard(&cancel, source, async { sftp.fs().metadata(source).await }).await?;
        match meta.file_type() {
            Some(t) if t.is_dir() => {
                plan.push(DownloadItem::Dir(local.clone()));
                scan_remote(job, source, &local, &mut plan).await?;
            }
            Some(t) if t.is_file() => plan.push(DownloadItem::File {
                remote: source.clone(),
                local,
                size: meta.len().unwrap_or(0),
            }),
            _ => job.skipped.push(source.clone()),
        }
    }

    job.total = plan
        .iter()
        .map(|item| match item {
            DownloadItem::File { size, .. } => *size,
            DownloadItem::Dir(_) => 0,
        })
        .sum();
    job.progress("", true);

    for item in plan {
        match item {
            DownloadItem::Dir(path) => {
                let context = path.display().to_string();
                guard(&cancel, &context, tokio::fs::create_dir_all(&path)).await?;
            }
            DownloadItem::File { remote, local, .. } => {
                download_file(job, &remote, &local).await?;
            }
        }
    }
    job.progress("", true);
    Ok(())
}

async fn scan_remote(
    job: &mut Job,
    remote_root: &str,
    local_root: &Path,
    plan: &mut Vec<DownloadItem>,
) -> Step<()> {
    let cancel = job.cancel.clone();
    let sftp = Arc::clone(&job.sftp);
    let mut pending = vec![(remote_root.to_string(), local_root.to_path_buf())];
    while let Some((remote_dir, local_dir)) = pending.pop() {
        let entries = guard(&cancel, &remote_dir, list_dir(&sftp, &remote_dir)).await?;
        for entry in entries {
            let local = local_dir.join(&entry.name);
            match (entry.kind, entry.is_link) {
                // Ссылки на каталоги не обходятся: так не бывает циклов.
                (EntryKind::Dir, true) | (EntryKind::Other, _) => job.skipped.push(entry.path),
                (EntryKind::Dir, false) => {
                    plan.push(DownloadItem::Dir(local.clone()));
                    pending.push((entry.path, local));
                }
                (EntryKind::File, _) => plan.push(DownloadItem::File {
                    remote: entry.path,
                    local,
                    size: entry.size.unwrap_or(0),
                }),
            }
        }
    }
    Ok(())
}

/// Скачивает файл во временный сосед и переименовывает по готовности, чтобы
/// прерванная передача не оставляла испорченный файл под настоящим именем.
async fn download_file(job: &mut Job, remote: &str, local: &Path) -> Step<()> {
    let part = part_path(local);
    let result = async {
        copy_down(job, remote, &part).await?;
        let context = local.display().to_string();
        guard(&job.cancel, &context, tokio::fs::rename(&part, local)).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    result
}

fn part_path(local: &Path) -> PathBuf {
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    local.with_file_name(format!(".{name}.sshdeck-part"))
}

async fn copy_down(job: &mut Job, remote: &str, part: &Path) -> Step<()> {
    let cancel = job.cancel.clone();
    let name = base_name(remote).to_string();

    let file = guard(&cancel, remote, job.sftp.open(remote)).await?;
    let reader = TokioCompatFile::new(file);
    tokio::pin!(reader);

    let part_context = part.display().to_string();
    let mut writer = guard(&cancel, &part_context, tokio::fs::File::create(part)).await?;

    let mut buf = vec![0u8; BUFFER_SIZE];
    loop {
        let n = guard(&cancel, remote, reader.read(&mut buf)).await?;
        if n == 0 {
            break;
        }
        guard(&cancel, &part_context, writer.write_all(&buf[..n])).await?;
        job.done += n as u64;
        job.progress(&name, false);
    }
    guard(&cancel, &part_context, writer.flush()).await?;
    Ok(())
}

// ------------------------------------------------------------------ загрузка

enum UploadItem {
    Dir(String),
    File {
        local: PathBuf,
        remote: String,
        size: u64,
    },
}

pub(super) async fn upload(job: &mut Job, sources: Vec<PathBuf>, dest_dir: String) -> Step<()> {
    let cancel = job.cancel.clone();
    let mut plan = Vec::new();

    for source in &sources {
        let context = source.display().to_string();
        let Some(name) = source.file_name().and_then(|n| n.to_str()) else {
            job.skipped.push(context);
            continue;
        };
        let remote = join_remote(&dest_dir, name);
        let meta = guard(&cancel, &context, tokio::fs::metadata(source)).await?;
        if meta.is_dir() {
            plan.push(UploadItem::Dir(remote.clone()));
            scan_local(job, source, &remote, &mut plan).await?;
        } else if meta.is_file() {
            plan.push(UploadItem::File {
                local: source.clone(),
                remote,
                size: meta.len(),
            });
        } else {
            job.skipped.push(context);
        }
    }
    run_upload_plan(job, plan).await
}

/// Заливает один файл по точному удалённому пути.
pub(super) async fn upload_file(job: &mut Job, local: PathBuf, remote: String) -> Step<()> {
    let context = local.display().to_string();
    let meta = guard(&job.cancel, &context, tokio::fs::metadata(&local)).await?;
    let plan = vec![UploadItem::File {
        local,
        remote,
        size: meta.len(),
    }];
    run_upload_plan(job, plan).await
}

async fn run_upload_plan(job: &mut Job, plan: Vec<UploadItem>) -> Step<()> {
    job.total = plan
        .iter()
        .map(|item| match item {
            UploadItem::File { size, .. } => *size,
            UploadItem::Dir(_) => 0,
        })
        .sum();
    job.progress("", true);

    for item in plan {
        match item {
            UploadItem::Dir(remote) => ensure_remote_dir(job, &remote).await?,
            UploadItem::File { local, remote, .. } => upload_one(job, &local, &remote).await?,
        }
    }
    job.progress("", true);
    Ok(())
}

async fn scan_local(
    job: &mut Job,
    local_root: &Path,
    remote_root: &str,
    plan: &mut Vec<UploadItem>,
) -> Step<()> {
    let cancel = job.cancel.clone();
    let mut pending = vec![(local_root.to_path_buf(), remote_root.to_string())];
    while let Some((local_dir, remote_dir)) = pending.pop() {
        let context = local_dir.display().to_string();
        let mut reader = guard(&cancel, &context, tokio::fs::read_dir(&local_dir)).await?;
        while let Some(entry) = guard(&cancel, &context, reader.next_entry()).await? {
            let path = entry.path();
            let entry_context = path.display().to_string();
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                job.skipped.push(entry_context);
                continue;
            };
            let remote = join_remote(&remote_dir, &name);
            let file_type = guard(&cancel, &entry_context, entry.file_type()).await?;
            if file_type.is_dir() {
                plan.push(UploadItem::Dir(remote.clone()));
                pending.push((path, remote));
            } else if file_type.is_file() {
                let meta = guard(&cancel, &entry_context, entry.metadata()).await?;
                plan.push(UploadItem::File {
                    local: path,
                    remote,
                    size: meta.len(),
                });
            } else if file_type.is_symlink() {
                // Ссылка на файл загружается как файл; ссылки на каталоги и
                // битые ссылки пропускаются.
                match tokio::fs::metadata(&path).await {
                    Ok(meta) if meta.is_file() => plan.push(UploadItem::File {
                        local: path,
                        remote,
                        size: meta.len(),
                    }),
                    _ => job.skipped.push(entry_context),
                }
            } else {
                job.skipped.push(entry_context);
            }
        }
    }
    Ok(())
}

async fn ensure_remote_dir(job: &mut Job, remote: &str) -> Step<()> {
    let sftp = Arc::clone(&job.sftp);
    guard(&job.cancel, remote, async {
        let mut fs = sftp.fs();
        match fs.create_dir(remote).await {
            Ok(()) => Ok(()),
            // Каталог уже есть — это не ошибка.
            Err(err) => match fs.metadata(remote).await {
                Ok(meta) if meta.file_type().is_some_and(|t| t.is_dir()) => Ok(()),
                _ => Err(err),
            },
        }
    })
    .await
}

/// Пишет прямо в целевой файл (с усечением), а не через временный: так у
/// существующего файла сохраняются владелец и права.
async fn upload_one(job: &mut Job, local: &Path, remote: &str) -> Step<()> {
    let sftp = Arc::clone(&job.sftp);
    let existed = sftp.fs().symlink_metadata(remote).await.is_ok();
    let result = copy_up(job, local, remote).await;
    if result.is_err() && !existed {
        // Недописанный новый файл убираем; если не вышло — не страшно.
        let cleanup = async { sftp.fs().remove_file(remote).await };
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, cleanup).await;
    }
    result
}

async fn copy_up(job: &mut Job, local: &Path, remote: &str) -> Step<()> {
    let cancel = job.cancel.clone();
    let name = base_name(remote).to_string();
    let local_context = local.display().to_string();

    let mut reader = guard(&cancel, &local_context, tokio::fs::File::open(local)).await?;
    let mut options = job.sftp.options();
    options.write(true).create(true).truncate(true);
    let mut writer = guard(&cancel, remote, options.open(remote)).await?;

    let mut buf = vec![0u8; BUFFER_SIZE];
    loop {
        let n = guard(&cancel, &local_context, reader.read(&mut buf)).await?;
        if n == 0 {
            break;
        }
        guard(&cancel, remote, writer.write_all(&buf[..n])).await?;
        job.done += n as u64;
        job.progress(&name, false);
    }
    guard(&cancel, remote, writer.close()).await?;
    Ok(())
}
