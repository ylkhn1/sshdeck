//! Установка расширений без ручной работы: скачанный файл, архив, папка
//! или ссылка. Программа сама распаковывает архив, делает файл
//! исполняемым, проверяет, что это расширение sshdeck, и кладёт его в
//! каталог расширений под его `id`. Новая версия заменяет старую, даже
//! если та лежала под другим именем.
//!
//! Файлы `sshdeck-driver-*` в «Загрузках» предлагаются к установке сами,
//! но запускаются только по согласию: расширение — это программа.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    CacheKey, ExtensionInfo, PATH_PREFIX, cache, drivers_dir, info_for, is_executable, query_info,
    scan,
};

/// Больше расширение весить не может (защита от ссылки не туда).
const MAX_DOWNLOAD: u64 = 100 * 1024 * 1024;
/// Какие файлы из «Загрузок» уже установлены или скрыты.
const SOURCES_FILE: &str = ".sources";
const SOURCES_KEEP: usize = 200;
/// Недокачанные файлы браузеров.
const PARTIAL: [&str; 5] = [".part", ".crdownload", ".download", ".partial", ".tmp"];

/// Устанавливает расширение из файла (программа или архив) или папки.
pub fn install_file(source: &Path) -> Result<ExtensionInfo, String> {
    let info = install_into(&drivers_dir(), source)?;
    remember_source(&drivers_dir(), source);
    scan();
    Ok(info)
}

/// Скачивает расширение по ссылке и устанавливает его.
pub fn install_url(url: &str) -> Result<ExtensionInfo, String> {
    let dir = drivers_dir();
    let work = Staging::new(&dir)?;
    let file = download(url, work.path())?;
    let info = install_into(&dir, &file)?;
    scan();
    Ok(info)
}

/// Расширение лежит в каталоге пользователя (его можно удалить из
/// программы), а не в системном.
pub fn is_removable(info: &ExtensionInfo) -> bool {
    entry_of(&drivers_dir(), &info.path).is_some()
}

pub fn uninstall(info: &ExtensionInfo) -> Result<(), String> {
    let entry = entry_of(&drivers_dir(), &info.path)
        .ok_or("Это расширение установлено в системе — удалить его можно менеджером пакетов")?;
    remove_entry(&entry).map_err(|err| format!("Не удалось удалить {}: {err}", entry.display()))?;
    scan();
    Ok(())
}

/// Скачанный файл, похожий на расширение.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
}

impl Offer {
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Каталог загрузок пользователя.
pub fn downloads_dir() -> Option<PathBuf> {
    gtk::glib::user_special_dir(gtk::glib::UserDirectory::Downloads)
        .or_else(|| Some(gtk::glib::home_dir().join("Downloads")))
}

/// Файлы `sshdeck-driver-*` в «Загрузках», которые ещё не ставили и не
/// скрывали; новые первыми. Сами файлы не запускаются.
pub fn download_offers() -> Vec<Offer> {
    match downloads_dir() {
        Some(downloads) => offers_in(&downloads, &drivers_dir()),
        None => Vec::new(),
    }
}

/// Больше не предлагать этот файл (пока он не изменится).
pub fn dismiss_offer(path: &Path) {
    remember_source(&drivers_dir(), path);
}

// ------------------------------------------------------------------ установка

/// Временный каталог внутри каталога расширений (та же файловая система —
/// перенос на место атомарный). Имя с точкой: при поиске расширений он не
/// виден. Удаляется вместе со всем содержимым.
struct Staging(PathBuf);

impl Staging {
    fn new(dir: &Path) -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        fs::create_dir_all(dir)
            .map_err(|err| format!("Не удалось создать {}: {err}", dir.display()))?;
        let path = dir.join(format!(
            ".install-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path)
            .map_err(|err| format!("Не удалось создать {}: {err}", path.display()))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn install_into(dir: &Path, source: &Path) -> Result<ExtensionInfo, String> {
    let staging = Staging::new(dir)?;
    let package = stage(source, staging.path())?;
    let driver = driver_in(&package);
    let mut info = query_info(&driver).map_err(|err| not_extension(&err))?;
    let target = dir.join(&info.id);
    remove_same_id(dir, &info.id, &target);
    replace(&package, &target, staging.path())?;
    let driver = driver_in(&target);
    info.path = driver.clone();
    // Переименование сохраняет время изменения: поиск возьмёт описание из
    // кеша и не будет запускать программу ещё раз.
    if let Ok(meta) = fs::metadata(&driver)
        && let Ok(mut cache) = cache().lock()
    {
        let key: CacheKey = (meta.modified().ok(), meta.len());
        cache.insert(driver, (key, Some(info.clone())));
    }
    Ok(info)
}

/// Программа расширения: сам файл или `driver` в каталоге.
fn driver_in(package: &Path) -> PathBuf {
    if package.is_dir() {
        package.join("driver")
    } else {
        package.to_path_buf()
    }
}

fn not_extension(error: &str) -> String {
    let reason = if error.contains("os error 8") {
        "файл не является программой (нет строки #! в начале или он собран для другой системы)"
    } else {
        error
    };
    format!("Это не расширение sshdeck: {reason}")
}

/// Готовит пакет во временном каталоге: исполняемый файл или каталог с
/// исполняемым `driver`.
fn stage(source: &Path, work: &Path) -> Result<PathBuf, String> {
    let meta = fs::metadata(source).map_err(|err| format!("{}: {err}", source.display()))?;
    if meta.is_dir() {
        let package = work.join("package");
        copy_dir(source, &package)
            .map_err(|err| format!("Не удалось скопировать {}: {err}", source.display()))?;
        return package_in(&package);
    }
    let head = read_head(source).map_err(|err| format!("{}: {err}", source.display()))?;
    if is_archive(&head) {
        let package = work.join("package");
        fs::create_dir(&package).map_err(|err| err.to_string())?;
        extract(source, &head, &package)?;
        return package_in(&package);
    }
    if is_html(&head) {
        return Err(
            "Это веб-страница, а не расширение. Нужен сам файл: на GitHub — \
                    кнопка «Raw» или «Download raw file»."
                .to_string(),
        );
    }
    let file = work.join("driver");
    fs::copy(source, &file).map_err(|err| format!("Не удалось скопировать файл: {err}"))?;
    make_executable(&file)?;
    Ok(file)
}

fn read_head(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(512);
    fs::File::open(path)?.take(512).read_to_end(&mut head)?;
    Ok(head)
}

fn is_zip(head: &[u8]) -> bool {
    head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06")
}

/// Архив по содержимому, а не по расширению имени (браузер мог его
/// переименовать).
fn is_archive(head: &[u8]) -> bool {
    is_zip(head)
        || head.starts_with(&[0x1f, 0x8b])
        || head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00])
        || head.starts_with(b"BZh")
        || head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
        || head.starts_with(&[b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c])
        || head.get(257..262) == Some(b"ustar".as_slice())
}

fn is_html(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head).trim_start().to_lowercase();
    text.starts_with("<!doctype html") || text.starts_with("<html")
}

/// Распаковывает архив. `bsdtar` понимает и zip, и tar любого сжатия и по
/// умолчанию не пишет за пределы каталога; без него — `unzip` или `tar`.
fn extract(archive: &Path, head: &[u8], dest: &Path) -> Result<(), String> {
    let a = archive.as_os_str();
    let d = dest.as_os_str();
    let mut attempts: Vec<(&str, Vec<&std::ffi::OsStr>)> =
        vec![("bsdtar", vec!["-xf".as_ref(), a, "-C".as_ref(), d])];
    if is_zip(head) {
        attempts.push((
            "unzip",
            vec!["-qq".as_ref(), "-o".as_ref(), a, "-d".as_ref(), d],
        ));
    } else {
        attempts.push((
            "tar",
            vec![
                "-xf".as_ref(),
                a,
                "-C".as_ref(),
                d,
                "--no-same-owner".as_ref(),
            ],
        ));
    }
    let mut last_error = None;
    for (program, args) in attempts {
        match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
        {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                last_error = Some(String::from_utf8_lossy(&output.stderr).trim().to_string());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => last_error = Some(err.to_string()),
        }
    }
    Err(match last_error {
        Some(error) => format!("Не удалось распаковать архив: {error}"),
        None => "Не удалось распаковать архив: нужна программа bsdtar, tar или unzip".to_string(),
    })
}

/// Видимые элементы каталога (без скрытых и служебного `__MACOSX`).
fn visible_entries(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|err| err.to_string())?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                !name.starts_with('.') && name != "__MACOSX"
            })
        })
        .collect();
    entries.sort();
    Ok(entries)
}

/// Находит программу в распакованном архиве: `driver`, единственный
/// `sshdeck-driver-*` или единственный файл. Одиночные вложенные каталоги
/// (архив «папка с файлами») пропускаются.
fn package_in(package: &Path) -> Result<PathBuf, String> {
    let mut root = package.to_path_buf();
    loop {
        let entries = visible_entries(&root)?;
        match entries.as_slice() {
            [only] if only.is_dir() && !root.join("driver").is_file() => root = only.clone(),
            _ => break,
        }
    }
    let driver = root.join("driver");
    if !driver.is_file() {
        let files: Vec<PathBuf> = visible_entries(&root)?
            .into_iter()
            .filter(|path| path.is_file())
            .collect();
        let named: Vec<&PathBuf> = files
            .iter()
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(PATH_PREFIX))
            })
            .collect();
        let chosen = match (named.as_slice(), files.as_slice()) {
            ([one], _) => (*one).clone(),
            ([], [one]) => one.clone(),
            _ => {
                return Err(
                    "Не найдена программа расширения: в архиве должен быть файл \
                            driver или sshdeck-driver-<имя>"
                        .to_string(),
                );
            }
        };
        fs::rename(&chosen, &driver).map_err(|err| err.to_string())?;
    }
    make_executable(&driver)?;
    Ok(root)
}

fn make_executable(path: &Path) -> Result<(), String> {
    let meta = fs::metadata(path).map_err(|err| err.to_string())?;
    let mut permissions = meta.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions)
        .map_err(|err| format!("Не удалось сделать файл исполняемым: {err}"))
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn remove_entry(path: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Убирает прежние копии того же расширения под другими именами, чтобы
/// после обновления не осталось двух версий.
fn remove_same_id(dir: &Path, id: &str, keep: &Path) {
    let Ok(entries) = visible_entries(dir) else {
        return;
    };
    for entry in entries {
        if entry == keep {
            continue;
        }
        let driver = driver_in(&entry);
        if is_executable(&driver) && info_for(&driver).is_some_and(|info| info.id == id) {
            let _ = remove_entry(&entry);
        }
    }
}

/// Кладёт пакет на место `target`. Старая версия убирается в сторону и
/// возвращается, если перенос не удался.
fn replace(package: &Path, target: &Path, staging: &Path) -> Result<(), String> {
    let previous = staging.join("previous");
    let had_previous = fs::symlink_metadata(target).is_ok();
    if had_previous {
        fs::rename(target, &previous)
            .map_err(|err| format!("Не удалось заменить {}: {err}", target.display()))?;
    }
    if let Err(err) = fs::rename(package, target) {
        if had_previous {
            let _ = fs::rename(&previous, target);
        }
        return Err(format!(
            "Не удалось установить в {}: {err}",
            target.display()
        ));
    }
    Ok(())
}

/// Элемент каталога расширений, которому принадлежит программа: сам файл
/// или его каталог. `None` — программа лежит не в `dir`.
fn entry_of(dir: &Path, driver: &Path) -> Option<PathBuf> {
    let parent = driver.parent()?;
    if parent == dir {
        return Some(driver.to_path_buf());
    }
    (driver.file_name()? == "driver" && parent.parent()? == dir).then(|| parent.to_path_buf())
}

// ------------------------------------------------------------------- загрузка

/// Ссылку на страницу файла GitHub превращает в ссылку на сам файл.
fn direct_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://github.com/") {
        let parts: Vec<&str> = rest.splitn(4, '/').collect();
        if let [owner, repo, "blob", path] = parts.as_slice() {
            return format!("https://raw.githubusercontent.com/{owner}/{repo}/{path}");
        }
    }
    url.to_string()
}

/// Имя файла из ссылки (для сообщений и чтобы `sshdeck-driver-*` осталось
/// узнаваемым), только безопасные символы.
fn url_file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let name: String = path
        .rsplit('/')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    match name.trim_start_matches('.') {
        "" => "download".to_string(),
        name => name.to_string(),
    }
}

fn download(url: &str, work: &Path) -> Result<PathBuf, String> {
    let url = url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("Нужна ссылка, начинающаяся с https:// или http://".to_string());
    }
    let url = direct_url(url);
    let file = work.join(url_file_name(&url));
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--proto",
            "=https,http",
            "--proto-redir",
            "=https,http",
        ])
        .args(["--connect-timeout", "20", "--max-time", "300"])
        .arg("--max-filesize")
        .arg(MAX_DOWNLOAD.to_string())
        .arg("-o")
        .arg(&file)
        .arg("--")
        .arg(&url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                "Для загрузки по ссылке нужна программа curl".to_string()
            } else {
                format!("Не удалось запустить curl: {err}")
            }
        })?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(download_error(output.status.code(), &error));
    }
    Ok(file)
}

/// Понятное сообщение по коду выхода curl.
fn download_error(code: Option<i32>, stderr: &str) -> String {
    let detail = stderr.trim().trim_start_matches("curl: ");
    let reason = match code {
        Some(6) => "сервер не найден — проверьте адрес и интернет".to_string(),
        Some(7) => "сервер не отвечает".to_string(),
        Some(28) => "сервер слишком долго не отвечает".to_string(),
        Some(35 | 60) => format!("ошибка защищённого соединения ({detail})"),
        Some(63) => "файл больше 100 МБ — это не похоже на расширение".to_string(),
        Some(22) if detail.contains("404") => "по ссылке ничего нет (ошибка 404)".to_string(),
        Some(22) if detail.contains("403") || detail.contains("401") => {
            "нет доступа к файлу (ссылка требует входа)".to_string()
        }
        _ => detail.to_string(),
    };
    format!("Не удалось скачать: {reason}")
}

// --------------------------------------------------------- предложения из загрузок

/// Отпечаток файла: путь, размер и время изменения. Скачали новую версию
/// под тем же именем — отпечаток другой, и она снова предлагается.
fn fingerprint(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    Some(format!("{}\t{}\t{modified}", path.display(), meta.len()))
}

fn read_sources(drivers: &Path) -> Vec<String> {
    fs::read_to_string(drivers.join(SOURCES_FILE))
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn remember_source(drivers: &Path, source: &Path) {
    let Some(print) = fingerprint(source) else {
        return;
    };
    let mut lines = read_sources(drivers);
    if lines.contains(&print) {
        return;
    }
    lines.push(print);
    let skip = lines.len().saturating_sub(SOURCES_KEEP);
    let text: String = lines[skip..]
        .iter()
        .map(|line| format!("{line}\n"))
        .collect();
    let _ = fs::create_dir_all(drivers);
    let path = drivers.join(SOURCES_FILE);
    let temp = drivers.join(format!("{SOURCES_FILE}.{}", std::process::id()));
    let written = fs::File::create(&temp).and_then(|mut file| file.write_all(text.as_bytes()));
    if written.is_ok() {
        let _ = fs::rename(&temp, &path);
    } else {
        let _ = fs::remove_file(&temp);
    }
}

fn offers_in(downloads: &Path, drivers: &Path) -> Vec<Offer> {
    let Ok(entries) = fs::read_dir(downloads) else {
        return Vec::new();
    };
    let seen = read_sources(drivers);
    let mut offers: Vec<Offer> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy().to_lowercase();
                name.starts_with("sshdeck-driver")
                    && !PARTIAL.iter().any(|suffix| name.ends_with(suffix))
            })
        })
        .filter_map(|path| {
            let meta = fs::metadata(&path).ok()?;
            if !(meta.is_dir() || (meta.is_file() && meta.len() > 0)) {
                return None;
            }
            if seen.contains(&fingerprint(&path)?) {
                return None;
            }
            Some(Offer {
                modified: meta.modified().ok(),
                path,
            })
        })
        .collect();
    offers.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.path.cmp(&b.path)));
    offers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_driver(id: &str, name: &str) -> String {
        format!(
            "#!/bin/sh\n[ \"$1\" = \"--sshdeck-info\" ] && \
             echo '{{\"sshdeck_driver\":1,\"id\":\"{id}\",\"name\":\"{name}\"}}'\n"
        )
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".install"))
            .collect()
    }

    /// Скачанный скрипт без права на запуск ставится как `<id>`, новая
    /// версия заменяет старую, даже если та лежала под другим именем.
    #[test]
    fn installs_plain_file_and_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let drivers = tmp.path().join("drivers");
        let download = tmp.path().join("dl/sshdeck-driver-foo");
        write(&download, &fake_driver("foo", "Foo 1"));
        let info = install_into(&drivers, &download).expect("install");
        assert_eq!(info.id, "foo");
        assert_eq!(info.path, drivers.join("foo"));
        assert!(is_executable(&drivers.join("foo")));

        // Старая копия под другим именем исчезает при обновлении.
        let old = drivers.join("my-foo");
        write(&old, &fake_driver("foo", "Foo 0"));
        make_executable(&old).unwrap();
        write(&download, &fake_driver("foo", "Foo 2"));
        let info = install_into(&drivers, &download).expect("update");
        assert_eq!(info.name, "Foo 2");
        assert!(!old.exists());
        assert!(
            fs::read_to_string(drivers.join("foo"))
                .unwrap()
                .contains("Foo 2")
        );
        assert!(leftovers(&drivers).is_empty());
    }

    /// Архивы: zip с папкой и `sshdeck-driver-*` внутри (права теряются),
    /// tar.gz с `driver`. Соседние файлы остаются рядом с программой.
    #[test]
    fn installs_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let drivers = tmp.path().join("drivers");
        let src = tmp.path().join("src");
        write(
            &src.join("pkg/sshdeck-driver-bar"),
            &fake_driver("bar", "Bar"),
        );
        write(&src.join("pkg/helper.txt"), "data");
        let zip = tmp.path().join("bar.download-name");
        let status = Command::new("bsdtar")
            .args(["--format", "zip", "-cf"])
            .arg(&zip)
            .arg("-C")
            .arg(&src)
            .arg("pkg")
            .status()
            .unwrap();
        assert!(status.success());
        let info = install_into(&drivers, &zip).expect("zip");
        assert_eq!(info.path, drivers.join("bar/driver"));
        assert!(is_executable(&info.path));
        assert!(drivers.join("bar/helper.txt").is_file());

        write(&src.join("baz/driver"), &fake_driver("baz", "Baz"));
        let tgz = tmp.path().join("baz.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&tgz)
            .arg("-C")
            .arg(&src)
            .arg("baz")
            .status()
            .unwrap();
        assert!(status.success());
        let info = install_into(&drivers, &tgz).expect("tgz");
        assert_eq!(info.path, drivers.join("baz/driver"));

        // Папка (перетащили каталог) ставится так же.
        write(
            &src.join("dir/sshdeck-driver-qux"),
            &fake_driver("qux", "Qux"),
        );
        let info = install_into(&drivers, &src.join("dir")).expect("dir");
        assert_eq!(info.path, drivers.join("qux/driver"));
        assert!(leftovers(&drivers).is_empty());
    }

    /// Не расширение — понятная ошибка, в каталоге ничего не остаётся.
    #[test]
    fn rejects_non_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        let drivers = tmp.path().join("drivers");
        let text = tmp.path().join("notes.txt");
        write(&text, "просто текст\n");
        let error = install_into(&drivers, &text).expect_err("text");
        assert!(error.starts_with("Это не расширение sshdeck"), "{error}");
        let script = tmp.path().join("script");
        write(&script, "#!/bin/sh\necho hello\n");
        let error = install_into(&drivers, &script).expect_err("script");
        assert!(error.contains("нет описания JSON"), "{error}");
        let page = tmp.path().join("page");
        write(&page, "\n<!DOCTYPE html><html></html>");
        let error = install_into(&drivers, &page).expect_err("html");
        assert!(error.contains("веб-страница"), "{error}");
        let src = tmp.path().join("src");
        write(&src.join("a"), "1");
        write(&src.join("b"), "2");
        let error = install_into(&drivers, &src).expect_err("ambiguous");
        assert!(error.contains("Не найдена программа"), "{error}");
        assert!(fs::read_dir(&drivers).unwrap().next().is_none());
    }

    #[test]
    fn entries_and_urls() {
        let dir = Path::new("/d");
        assert_eq!(
            entry_of(dir, Path::new("/d/foo")),
            Some(PathBuf::from("/d/foo"))
        );
        assert_eq!(
            entry_of(dir, Path::new("/d/foo/driver")),
            Some(PathBuf::from("/d/foo"))
        );
        assert_eq!(entry_of(dir, Path::new("/usr/lib/x/foo")), None);
        assert_eq!(entry_of(dir, Path::new("/d/foo/bin/x")), None);
        assert_eq!(
            direct_url("https://github.com/o/r/blob/main/ext/sshdeck-driver-x"),
            "https://raw.githubusercontent.com/o/r/main/ext/sshdeck-driver-x"
        );
        assert_eq!(direct_url("https://example.com/a"), "https://example.com/a");
        assert_eq!(
            url_file_name("https://x/y/sshdeck-driver-ch.zip?raw=1"),
            "sshdeck-driver-ch.zip"
        );
        assert_eq!(url_file_name("https://x/"), "download");
        assert_eq!(url_file_name("https://x/..%2f"), "2f");
        assert!(download("ftp://x/y", Path::new("/nonexistent")).is_err());
        assert_eq!(
            download_error(
                Some(22),
                "curl: (22) The requested URL returned error: 404\n"
            ),
            "Не удалось скачать: по ссылке ничего нет (ошибка 404)"
        );
        assert!(download_error(Some(99), "curl: (99) odd").ends_with("(99) odd"));
    }

    /// Предложения из «Загрузок»: только `sshdeck-driver-*`, без
    /// недокачанных; установленный или скрытый файл больше не предлагается,
    /// пока его не скачают заново.
    #[test]
    fn download_offers_list() {
        let tmp = tempfile::tempdir().unwrap();
        let downloads = tmp.path().join("dl");
        let drivers = tmp.path().join("drivers");
        write(&downloads.join("sshdeck-driver-a"), "x");
        write(&downloads.join("sshdeck-driver-b.zip.part"), "x");
        write(&downloads.join("photo.jpg"), "x");
        write(&downloads.join("sshdeck-driver-empty"), "");
        let offers = offers_in(&downloads, &drivers);
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].file_name(), "sshdeck-driver-a");
        remember_source(&drivers, &offers[0].path);
        assert!(offers_in(&downloads, &drivers).is_empty());
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&downloads.join("sshdeck-driver-a"), "xy");
        assert_eq!(offers_in(&downloads, &drivers).len(), 1);
    }
}
