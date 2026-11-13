//! Загрузка и установка обновления.
//!
//! Файл выпуска и его подпись скачиваются в `~/.cache/sshdeck/updates/`,
//! подпись проверяется до того, как файл используется. Дальше:
//!
//! - архив: из него берётся только `sshdeck-X.Y.Z/sshdeck` (в память, с
//!   ограничением размера; ничего другого из архива на диск не пишется),
//!   новый файл программы пишется рядом со старым и переименовывается
//!   поверх него — атомарно, работающая программа не страдает;
//! - пакет: `pkexec apt-get install -y <путь>` / `pkexec dnf install -y
//!   <путь>`; если не вышло — команда для терминала.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use super::{Endpoints, InstallKind, Offer, Version, curl_base, curl_reason, curl_spawn_error};

/// Больше файл выпуска весить не может.
const MAX_ASSET: u64 = 200 * 1024 * 1024;
/// Подпись minisign — несколько сотен байт.
const MAX_SIGNATURE: u64 = 16 * 1024;
/// Больше программа в архиве весить не может.
const MAX_BINARY: u64 = 256 * 1024 * 1024;

/// Этапы обновления для [`Progress::stage`].
pub const STAGE_DOWNLOAD: u8 = 0;
pub const STAGE_VERIFY: u8 = 1;
pub const STAGE_INSTALL: u8 = 2;

/// Ход обновления: этап, сколько скачано; флаг отмены. Общий для фонового
/// потока и окна.
#[derive(Clone, Default)]
pub struct Progress {
    pub stage: Arc<AtomicU8>,
    pub downloaded: Arc<AtomicU64>,
    pub cancel: Arc<AtomicBool>,
}

impl Progress {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Загрузка отменена пользователем — не ошибка, сообщать не нужно.
pub const CANCELLED: &str = "\u{0}cancelled";

/// Скачивает файл выпуска из `offer` с подписью, проверяет её ключом
/// `key` и возвращает путь к проверенному файлу. Непрошедший проверку
/// файл удаляется.
pub fn fetch_verified(
    offer: &Offer,
    endpoints: &Endpoints,
    dir: &Path,
    key: &str,
    progress: &Progress,
) -> Result<PathBuf, String> {
    let kind = offer.method.kind();
    let (name, _) = offer
        .asset
        .as_ref()
        .ok_or_else(|| tr!("This release has no file for this installation").to_string())?;
    let url = endpoints.download_url(offer.version, name);
    let sig_url = format!("{url}.minisig");
    endpoints.check_download_url(&url, offer.version, kind)?;
    endpoints.check_download_url(&sig_url, offer.version, kind)?;

    clean_downloads(dir, offer.version);
    let dir = dir.join(offer.version.to_string());
    fs::create_dir_all(&dir).map_err(|err| tr!("Could not create {}: {}", dir.display(), err))?;
    let file = dir.join(name);
    let sig_file = dir.join(format!("{name}.minisig"));
    download(
        endpoints,
        &sig_url,
        &sig_file,
        MAX_SIGNATURE,
        &Progress::default(),
    )?;
    progress.downloaded.store(0, Ordering::Relaxed);
    download(endpoints, &url, &file, MAX_ASSET, progress)?;
    progress.stage.store(STAGE_VERIFY, Ordering::Relaxed);
    let signature = fs::read_to_string(&sig_file).unwrap_or_default();
    let verified = super::verify::verify_file(&file, &signature, name, key);
    let _ = fs::remove_file(&sig_file);
    if let Err(err) = verified {
        let _ = fs::remove_file(&file);
        return Err(err);
    }
    Ok(file)
}

/// Убирает загрузки других версий (и недокачанные файлы).
fn clean_downloads(dir: &Path, keep: Version) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let keep = keep.to_string();
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy() != keep {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Скачивает `url` в `dest` через временный `.part`; ход загрузки — в
/// `progress`, отмена — по его флагу.
fn download(
    endpoints: &Endpoints,
    url: &str,
    dest: &Path,
    max_bytes: u64,
    progress: &Progress,
) -> Result<(), String> {
    let part = dest.with_extension("part");
    let _ = fs::remove_file(&part);
    let mut child = curl_base(endpoints, max_bytes, 900)
        .arg("-o")
        .arg(&part)
        .arg("--")
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| curl_spawn_error(&err))?;
    let status = loop {
        if progress.cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&part);
            return Err(CANCELLED.to_string());
        }
        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
            break status;
        }
        if let Ok(meta) = fs::metadata(&part) {
            progress.downloaded.store(meta.len(), Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    if !status.success() {
        let _ = fs::remove_file(&part);
        return Err(tr!(
            "Could not download: {}",
            curl_reason(status.code(), &stderr)
        ));
    }
    let size = fs::metadata(&part).map_or(0, |meta| meta.len());
    if size > max_bytes {
        let _ = fs::remove_file(&part);
        return Err(tr!(
            "Could not download: {}",
            tr!("the file is larger than expected")
        ));
    }
    progress.downloaded.store(size, Ordering::Relaxed);
    fs::rename(&part, dest).map_err(|err| tr!("Could not download: {}", err))
}

// ------------------------------------------------------------------- архив

/// Достаёт из архива выпуска `version` программу `sshdeck-X.Y.Z/sshdeck`.
/// Архив с путями наружу (`..`, абсолютными) отвергается целиком; прочие
/// файлы архива не читаются.
pub fn extract_binary(archive: &Path, version: Version) -> Result<Vec<u8>, String> {
    let broken = |reason: String| tr!("The update archive is damaged: {}", reason);
    let wanted = PathBuf::from(format!("sshdeck-{version}")).join("sshdeck");
    let file = fs::File::open(archive).map_err(|err| broken(err.to_string()))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut binary = None;
    for entry in tar.entries().map_err(|err| broken(err.to_string()))? {
        let mut entry = entry.map_err(|err| broken(err.to_string()))?;
        let path = entry
            .path()
            .map_err(|err| broken(err.to_string()))?
            .into_owned();
        if !is_safe_path(&path) {
            return Err(broken(tr!("unsafe path {}", path.display())));
        }
        if path != wanted {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(broken(tr!("{} is not a regular file", path.display())));
        }
        if binary.is_some() {
            return Err(broken(tr!("{} appears twice", path.display())));
        }
        if entry.size() > MAX_BINARY {
            return Err(broken(tr!("the file is larger than expected").to_string()));
        }
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry
            .by_ref()
            .take(MAX_BINARY + 1)
            .read_to_end(&mut data)
            .map_err(|err| broken(err.to_string()))?;
        binary = Some(data);
    }
    let binary = binary.ok_or_else(|| broken(tr!("{} is missing", wanted.display())))?;
    if !binary.starts_with(b"\x7fELF") {
        return Err(broken(tr!("{} is not a program", wanted.display())));
    }
    Ok(binary)
}

/// Относительный путь без `..`.
fn is_safe_path(path: &Path) -> bool {
    path.components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
}

// ------------------------------------------------------- замена программы

/// Можно ли заменить файл программы: он и его каталог принадлежат
/// пользователю, каталог доступен на запись и это не системный каталог.
/// От имени root программа себя не заменяет.
pub fn can_replace(exe: &Path) -> Result<(), String> {
    let refuse = || tr!("sshdeck cannot replace {}", exe.display());
    let dir = exe.parent().ok_or_else(refuse)?;
    // SAFETY: geteuid не имеет предусловий.
    let uid = unsafe { libc::geteuid() };
    if uid == 0 || !exe.is_absolute() {
        return Err(refuse());
    }
    for system in [
        "/usr", "/bin", "/sbin", "/lib", "/opt", "/etc", "/var", "/snap", "/nix",
    ] {
        if exe.starts_with(system) {
            return Err(refuse());
        }
    }
    let exe_meta = fs::symlink_metadata(exe).map_err(|_| refuse())?;
    let dir_meta = fs::metadata(dir).map_err(|_| refuse())?;
    if !exe_meta.is_file() || exe_meta.uid() != uid || dir_meta.uid() != uid {
        return Err(refuse());
    }
    let Ok(c_dir) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return Err(refuse());
    };
    // SAFETY: строка с нулём на конце живёт до конца вызова.
    if unsafe { libc::access(c_dir.as_ptr(), libc::W_OK) } != 0 {
        return Err(refuse());
    }
    Ok(())
}

/// Атомарно заменяет файл программы `exe` содержимым `binary`: временный
/// файл в том же каталоге, `fsync`, переименование поверх, `fsync`
/// каталога. Запущенная программа продолжает работать со старым файлом.
pub fn replace_executable(exe: &Path, binary: &[u8]) -> Result<(), String> {
    can_replace(exe)?;
    let failed = |err: std::io::Error| tr!("Could not replace {}: {}", exe.display(), err);
    let dir = exe
        .parent()
        .ok_or_else(|| tr!("sshdeck cannot replace {}", exe.display()))?;
    let name = exe
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = dir.join(format!(".{name}.update-{}", std::process::id()));
    let _ = fs::remove_file(&temp);
    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&temp)?;
        file.write_all(binary)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, exe)?;
        if let Ok(dir) = fs::File::open(dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if let Err(err) = written {
        let _ = fs::remove_file(&temp);
        return Err(failed(err));
    }
    Ok(())
}

// ------------------------------------------------------------------ пакеты

/// Менеджер пакетов и его аргументы для установки файла `package`.
fn package_manager(kind: InstallKind, package: &Path) -> Option<Vec<String>> {
    let manager = match kind {
        InstallKind::Deb => "apt-get",
        InstallKind::Rpm => "dnf",
        _ => return None,
    };
    // apt-get ставит локальный файл, только если в аргументе есть `/`:
    // путь всегда абсолютный.
    let path = std::path::absolute(package).ok()?;
    Some(vec![
        manager.to_string(),
        "install".to_string(),
        "-y".to_string(),
        path.to_string_lossy().into_owned(),
    ])
}

/// Команда установки через системный запрос пароля:
/// `pkexec apt-get install -y /…/sshdeck_X.Y.Z-1_amd64.deb`.
pub fn package_command(kind: InstallKind, package: &Path) -> Option<Vec<String>> {
    let mut command = vec!["pkexec".to_string()];
    command.extend(package_manager(kind, package)?);
    Some(command)
}

/// Та же установка для терминала: `sudo apt-get install -y '/…'`.
pub fn manual_command(kind: InstallKind, package: &Path) -> Option<String> {
    let mut words = vec!["sudo".to_string()];
    words.extend(
        package_manager(kind, package)?
            .iter()
            .map(|word| shell_quote(word)),
    );
    Some(words.join(" "))
}

fn shell_quote(word: &str) -> String {
    let plain = word
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_./=:+,@%".contains(&b));
    if plain && !word.is_empty() {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Программа вместо `pkexec`. В отладочной сборке её можно подменить
/// переменной `SSHDECK_UPDATE_PKEXEC` (только для проверок без запроса
/// пароля на настоящем рабочем столе).
fn pkexec_program() -> String {
    #[cfg(debug_assertions)]
    if let Ok(program) = std::env::var("SSHDECK_UPDATE_PKEXEC") {
        return program;
    }
    "pkexec".to_string()
}

/// Как закончилась установка пакета.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageResult {
    Installed,
    /// Пароль не ввели (окно запроса закрыли).
    Cancelled,
    /// pkexec нет или установка не удалась: текст ошибки.
    Failed(String),
}

/// Ставит пакет через `pkexec`. Долгий вызов — только из фонового потока.
pub fn install_package(kind: InstallKind, package: &Path) -> PackageResult {
    let Some(mut command) = package_command(kind, package) else {
        return PackageResult::Failed(tr!("This installation has no package manager").to_string());
    };
    command[0] = pkexec_program();
    let output = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match output {
        Ok(output) if output.status.success() => PackageResult::Installed,
        // 126: запрос пароля закрыли или в доступе отказано.
        Ok(output) if output.status.code() == Some(126) => PackageResult::Cancelled,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = lines[lines.len().saturating_sub(6)..].join("\n");
            PackageResult::Failed(match (output.status.code(), tail.is_empty()) {
                (Some(127), true) => tr!("no system password prompt is available").to_string(),
                (code, true) => tr!("exit code {}", code.unwrap_or(-1)),
                (_, false) => tail,
            })
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            PackageResult::Failed(tr!("pkexec is not installed").to_string())
        }
        Err(err) => PackageResult::Failed(err.to_string()),
    }
}

/// Удаляет скачанный пакет после установки.
pub fn forget_download(package: &Path) {
    let _ = fs::remove_file(package);
    if let Some(dir) = package.parent() {
        let _ = fs::remove_dir(dir);
    }
}

// ----------------------------------------------------------------- перезапуск

/// Запускает `exe` с теми же аргументами, что у этого запуска.
pub fn spawn_new(exe: &Path, args: &[String]) -> Result<(), String> {
    Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|err| tr!("Could not start {}: {}", exe.display(), err))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version() -> Version {
        Version::parse("0.5.0").unwrap()
    }

    /// Архив tar.gz с заданными файлами: (путь, содержимое). Пути пишутся
    /// в заголовок как есть, в обход проверок крейта tar.
    fn tarball(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join("release.tar.gz");
        let file = fs::File::create(&path).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(gz);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            {
                let bytes = header.as_old_mut();
                bytes.name[..name.len()].copy_from_slice(name.as_bytes());
            }
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        path
    }

    const ELF: &[u8] = b"\x7fELF fake program";

    #[test]
    fn binary_is_taken_from_the_versioned_directory() {
        let dir = tempfile::tempdir().unwrap();
        let archive = tarball(
            dir.path(),
            &[
                ("sshdeck-0.5.0/", b""),
                ("sshdeck-0.5.0/README.md", b"readme"),
                ("sshdeck-0.5.0/sshdeck", ELF),
                ("sshdeck-0.5.0/LICENSE", b"MIT"),
            ],
        );
        assert_eq!(extract_binary(&archive, version()).unwrap(), ELF);
        // Ничего, кроме самого архива, не появилось.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn archive_with_unsafe_paths_is_refused() {
        for evil in ["../sshdeck", "sshdeck-0.5.0/../../x", "/tmp/sshdeck"] {
            let dir = tempfile::tempdir().unwrap();
            let archive = tarball(dir.path(), &[(evil, ELF), ("sshdeck-0.5.0/sshdeck", ELF)]);
            let error = extract_binary(&archive, version()).unwrap_err();
            assert!(error.contains("unsafe path"), "{evil}: {error}");
        }
    }

    #[test]
    fn archive_without_the_right_program_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // Другая версия, программа в корне, не программа, дважды.
        let cases: [&[(&str, &[u8])]; 4] = [
            &[("sshdeck-0.4.0/sshdeck", ELF)],
            &[("sshdeck", ELF)],
            &[("sshdeck-0.5.0/sshdeck", b"#!/bin/sh\necho hi\n")],
            &[
                ("sshdeck-0.5.0/sshdeck", ELF),
                ("sshdeck-0.5.0/sshdeck", ELF),
            ],
        ];
        for entries in cases {
            let archive = tarball(dir.path(), entries);
            assert!(extract_binary(&archive, version()).is_err(), "{entries:?}");
        }
        let junk = dir.path().join("junk.tar.gz");
        fs::write(&junk, b"not an archive").unwrap();
        assert!(extract_binary(&junk, version()).is_err());
    }

    #[test]
    fn executable_is_replaced_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let exe = bin.join("sshdeck");
        fs::write(&exe, b"\x7fELF old").unwrap();
        // Запущенная программа держит старый файл открытым.
        let mut running = fs::File::open(&exe).unwrap();
        replace_executable(&exe, b"\x7fELF new").unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"\x7fELF new");
        let mode = fs::metadata(&exe).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o755);
        let mut old = Vec::new();
        running.read_to_end(&mut old).unwrap();
        assert_eq!(old, b"\x7fELF old");
        // Временных файлов не осталось.
        let names: Vec<_> = fs::read_dir(&bin)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["sshdeck"]);
    }

    #[test]
    fn system_and_foreign_locations_are_not_replaced() {
        assert!(can_replace(Path::new("/usr/bin/sshdeck")).is_err());
        assert!(can_replace(Path::new("/usr/local/bin/sshdeck")).is_err());
        assert!(can_replace(Path::new("/opt/sshdeck/sshdeck")).is_err());
        assert!(can_replace(Path::new("relative/sshdeck")).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(can_replace(&dir.path().join("missing")).is_err());
        // Каталог только для чтения.
        let exe = dir.path().join("sshdeck");
        fs::write(&exe, b"x").unwrap();
        assert!(can_replace(&exe).is_ok());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let readonly = can_replace(&exe);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(readonly.is_err());
        // Ссылка вместо файла не заменяется (current_exe её уже раскрыл).
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&exe, &link).unwrap();
        assert!(can_replace(&link).is_err());
    }

    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn package_commands() {
        let deb = Path::new("/home/u/.cache/sshdeck/updates/0.5.0/sshdeck_0.5.0-1_amd64.deb");
        assert_eq!(
            package_command(InstallKind::Deb, deb).unwrap(),
            [
                "pkexec",
                "apt-get",
                "install",
                "-y",
                "/home/u/.cache/sshdeck/updates/0.5.0/sshdeck_0.5.0-1_amd64.deb"
            ]
        );
        let rpm = Path::new("/home/u/.cache/sshdeck/updates/0.5.0/sshdeck-0.5.0-1.x86_64.rpm");
        assert_eq!(
            package_command(InstallKind::Rpm, rpm).unwrap(),
            [
                "pkexec",
                "dnf",
                "install",
                "-y",
                "/home/u/.cache/sshdeck/updates/0.5.0/sshdeck-0.5.0-1.x86_64.rpm"
            ]
        );
        assert_eq!(package_command(InstallKind::Tarball, rpm), None);
        assert_eq!(
            manual_command(InstallKind::Deb, deb).unwrap(),
            "sudo apt-get install -y /home/u/.cache/sshdeck/updates/0.5.0/sshdeck_0.5.0-1_amd64.deb"
        );
        // Пробелы и кавычки в пути экранируются.
        let odd = Path::new("/home/Мой дом/it's/sshdeck-0.5.0-1.x86_64.rpm");
        assert_eq!(
            manual_command(InstallKind::Rpm, odd).unwrap(),
            r"sudo dnf install -y '/home/Мой дом/it'\''s/sshdeck-0.5.0-1.x86_64.rpm'"
        );
        // Относительный путь становится абсолютным: иначе apt-get ищет
        // пакет с таким именем в репозиториях.
        let relative = package_command(InstallKind::Deb, Path::new("x.deb")).unwrap();
        assert!(relative[4].starts_with('/'), "{relative:?}");
    }
}
