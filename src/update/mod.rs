//! Проверка обновлений: последний выпуск на GitHub, сравнение версий и
//! то, как обновиться при этом способе установки.
//!
//! Способ установки зашит при сборке: `SSHDECK_INSTALL_KIND` = `deb`, `rpm`,
//! `aur` или `tarball` задают сборочные скрипты (`packaging/`). Без него
//! (`cargo build`) программа только сообщает о новой версии. При запуске
//! это ещё раз сверяется с тем, где лежит программа (см. [`method`]).
//!
//! - `tarball` — программа обновляет сама себя: скачивает архив и его
//!   подпись, проверяет, достаёт `sshdeck-X.Y.Z/sshdeck` и атомарно
//!   заменяет свой файл ([`install`]);
//! - `deb`, `rpm` — скачивает и проверяет пакет, ставит его через `pkexec`;
//! - `aur` и всё остальное — только сообщение и инструкция.
//!
//! Всё, что ставится, должно быть подписано ключом
//! `packaging/minisign.pub` ([`verify`]). Здесь нет GTK, кроме чтения
//! путей GLib; сетевые вызовы — через `curl`, из фоновых потоков.

pub mod install;
pub mod verify;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Репозиторий выпусков.
const REPO: &str = "ylkhn1/sshdeck";
const GITHUB_API: &str = "https://api.github.com";
const GITHUB: &str = "https://github.com";
/// Версия этой сборки.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Как часто проверять, секунды.
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
/// Ответ API больше этого — не то, что мы ждём.
const MAX_API_RESPONSE: u64 = 2 * 1024 * 1024;

// ------------------------------------------------------------------ версии

/// Версия выпуска `X.Y.Z`: только числа, без пред-выпусков (`-rc1`) и
/// метаданных сборки — их программа не предлагает.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version([u64; 3]);

impl Version {
    /// `0.5.0` или `v0.5.0`. `None` — не обычная версия выпуска.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = [0u64; 3];
        let mut count = 0;
        for part in text.split('.') {
            if count == 3
                || part.is_empty()
                || part.len() > 9
                || !part.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            parts[count] = part.parse().ok()?;
            count += 1;
        }
        (count == 3).then_some(Self(parts))
    }

    pub fn current() -> Self {
        Self::parse(CURRENT).unwrap_or(Self([0, 0, 0]))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [major, minor, patch] = self.0;
        write!(f, "{major}.{minor}.{patch}")
    }
}

// ------------------------------------------------------- способ установки

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    Tarball,
    Deb,
    Rpm,
    Aur,
    /// `cargo build`, `cargo run` или неизвестная сборка.
    Unknown,
}

impl InstallKind {
    pub fn from_code(code: Option<&str>) -> Self {
        match code.map(str::trim) {
            Some("tarball") => Self::Tarball,
            Some("deb") => Self::Deb,
            Some("rpm") => Self::Rpm,
            Some("aur") => Self::Aur,
            _ => Self::Unknown,
        }
    }

    /// Способ, зашитый при сборке. В отладочной сборке его можно
    /// подменить переменной `SSHDECK_UPDATE_KIND` (только для проверок).
    pub fn compiled() -> Self {
        #[cfg(debug_assertions)]
        if let Ok(kind) = std::env::var("SSHDECK_UPDATE_KIND") {
            return Self::from_code(Some(&kind));
        }
        Self::from_code(option_env!("SSHDECK_INSTALL_KIND"))
    }

    /// Имя файла выпуска для этого способа установки (только x86_64).
    pub fn asset_name(self, version: Version) -> Option<String> {
        match self {
            Self::Tarball => Some(format!("sshdeck-{version}-x86_64-linux.tar.gz")),
            Self::Deb => Some(format!("sshdeck_{version}-1_amd64.deb")),
            Self::Rpm => Some(format!("sshdeck-{version}-1.x86_64.rpm")),
            Self::Aur | Self::Unknown => None,
        }
    }
}

/// Как обновиться на этом компьютере.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// Заменить файл программы (архив из выпуска).
    SelfReplace(PathBuf),
    /// Поставить пакет через `pkexec` (`Deb` или `Rpm`).
    Package(InstallKind),
    /// Собрана из PKGBUILD: показать команды.
    Aur,
    /// Только ссылка на страницу выпуска.
    ReleasePage,
}

impl Method {
    /// Программа может поставить обновление сама.
    pub fn installs(&self) -> bool {
        matches!(self, Self::SelfReplace(_) | Self::Package(_))
    }

    pub fn kind(&self) -> InstallKind {
        match self {
            Self::SelfReplace(_) => InstallKind::Tarball,
            Self::Package(kind) => *kind,
            Self::Aur => InstallKind::Aur,
            Self::ReleasePage => InstallKind::Unknown,
        }
    }
}

/// Способ обновления по зашитому `kind` и пути программы `exe`, с
/// проверкой на месте: «архивная» программа, которая лежит в `/usr` или
/// в чужом каталоге, себя не заменяет; пакетная — только из `/usr/bin`
/// и при наличии менеджера пакетов. Готовые сборки — только для x86_64.
pub fn method(
    kind: InstallKind,
    exe: Option<&Path>,
    arch: &str,
    has_program: impl Fn(&str) -> bool,
) -> Method {
    let x86_64 = arch == "x86_64";
    match kind {
        InstallKind::Tarball if x86_64 => match exe {
            Some(exe) if install::can_replace(exe).is_ok() => {
                Method::SelfReplace(exe.to_path_buf())
            }
            _ => Method::ReleasePage,
        },
        InstallKind::Deb | InstallKind::Rpm if x86_64 => {
            let manager = if kind == InstallKind::Deb {
                "apt-get"
            } else {
                "dnf"
            };
            let packaged = exe.is_some_and(|exe| exe == Path::new("/usr/bin/sshdeck"));
            if packaged && has_program(manager) {
                Method::Package(kind)
            } else {
                Method::ReleasePage
            }
        }
        InstallKind::Aur => Method::Aur,
        _ => Method::ReleasePage,
    }
}

/// Путь программы при запуске. Запоминается заранее: после замены файла
/// `/proc/self/exe` указывает на удалённый файл.
pub fn startup_exe() -> Option<&'static Path> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().ok()).as_deref()
}

/// Есть ли программа в `PATH`.
pub fn in_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            std::fs::metadata(&candidate).is_ok_and(|meta| meta.is_file())
        })
    })
}

/// Способ обновления этой копии программы.
pub fn this_method() -> Method {
    let kind = InstallKind::compiled();
    // Проверка пакетной установки в отладочной сборке: она лежит не в
    // /usr/bin, а apt-get/dnf на машине разработчика может не быть.
    #[cfg(debug_assertions)]
    if std::env::var_os("SSHDECK_UPDATE_KIND").is_some()
        && matches!(kind, InstallKind::Deb | InstallKind::Rpm)
    {
        return Method::Package(kind);
    }
    method(kind, startup_exe(), std::env::consts::ARCH, in_path)
}

// -------------------------------------------------------------- адреса

/// Откуда брать сведения о выпусках и файлы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// `…/releases/latest` API GitHub.
    pub api: String,
    /// `https://github.com/<repo>/releases/download`.
    pub download: String,
    /// `https://github.com/<repo>/releases`.
    pub pages: String,
    /// Разрешён ли `http://` (только локальный тестовый сервер).
    pub allow_http: bool,
}

impl Endpoints {
    pub fn github() -> Self {
        Self::with_bases(GITHUB_API, GITHUB, false)
    }

    fn with_bases(api: &str, site: &str, allow_http: bool) -> Self {
        Self {
            api: format!("{api}/repos/{REPO}/releases/latest"),
            download: format!("{site}/{REPO}/releases/download"),
            pages: format!("{site}/{REPO}/releases"),
            allow_http,
        }
    }

    /// Настоящий GitHub. В отладочной сборке `SSHDECK_UPDATE_URL=<адрес>`
    /// направляет и API, и загрузки на тестовый сервер (только для
    /// проверок; в выпускной сборке переменная не действует).
    pub fn current() -> Self {
        #[cfg(debug_assertions)]
        if let Ok(base) = std::env::var("SSHDECK_UPDATE_URL") {
            let base = base.trim_end_matches('/');
            return Self::with_bases(base, base, true);
        }
        Self::github()
    }

    /// Страница выпуска с описанием изменений.
    pub fn release_page(&self, version: Version) -> String {
        format!("{}/tag/v{version}", self.pages)
    }

    /// Адрес файла выпуска; имя файла — только из [`InstallKind::asset_name`].
    pub fn download_url(&self, version: Version, name: &str) -> String {
        format!("{}/v{version}/{name}", self.download)
    }

    /// Пропускает только ожидаемый адрес: файл выпуска `version` для
    /// `kind` или его подпись `.minisig` — и ничего другого.
    pub fn check_download_url(
        &self,
        url: &str,
        version: Version,
        kind: InstallKind,
    ) -> Result<(), String> {
        let rejected = || tr!("Unexpected download address: {}", url);
        let name = kind.asset_name(version).ok_or_else(rejected)?;
        let asset = self.download_url(version, &name);
        if url != asset && url != format!("{asset}.minisig") {
            return Err(rejected());
        }
        let secure = url.starts_with("https://");
        if !(secure || (self.allow_http && url.starts_with("http://"))) {
            return Err(rejected());
        }
        Ok(())
    }
}

// --------------------------------------------------------------- выпуск

/// Последний выпуск по ответу API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// Файлы выпуска: имя и размер. Только чтобы узнать, есть ли нужный
    /// файл с подписью, и показать ход загрузки; адрес строится сам.
    pub assets: Vec<(String, u64)>,
}

impl Release {
    fn asset_size(&self, name: &str) -> Option<u64> {
        self.assets
            .iter()
            .find(|(asset, _)| asset == name)
            .map(|(_, size)| *size)
    }
}

/// Разбирает ответ `releases/latest`. `Ok(None)` — черновик, пред-выпуск
/// или тег не похож на версию: такие не предлагаются.
pub fn parse_release(json: &str) -> Result<Option<Release>, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|err| tr!("GitHub sent an unexpected answer: {}", err))?;
    let flag = |key: &str| value.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
    if flag("draft") || flag("prerelease") {
        return Ok(None);
    }
    let Some(tag) = value.get("tag_name").and_then(|v| v.as_str()) else {
        return Err(tr!("GitHub sent an unexpected answer: {}", "tag_name").to_string());
    };
    let Some(version) = Version::parse(tag) else {
        return Ok(None);
    };
    let assets = value
        .get("assets")
        .and_then(|v| v.as_array())
        .map(|assets| {
            assets
                .iter()
                .filter_map(|asset| {
                    let name = asset.get("name")?.as_str()?.to_string();
                    let size = asset.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                    Some((name, size))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(Release { version, assets }))
}

/// Что предложить пользователю.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub version: Version,
    pub method: Method,
    /// Для [`Method::installs`]: имя файла и размер из списка выпуска (0 —
    /// неизвестен).
    pub asset: Option<(String, u64)>,
}

/// Предложение по выпуску. Если нужного файла или его подписи в выпуске
/// нет, остаётся только ссылка на страницу.
pub fn offer(release: &Release, method: Method) -> Offer {
    let asset = method
        .kind()
        .asset_name(release.version)
        .filter(|_| method.installs())
        .filter(|name| release.asset_size(&format!("{name}.minisig")).is_some())
        .and_then(|name| release.asset_size(&name).map(|size| (name, size)));
    let method = if method.installs() && asset.is_none() {
        Method::ReleasePage
    } else {
        method
    };
    Offer {
        version: release.version,
        method,
        asset,
    }
}

/// Стоит ли сообщать о `latest`: она новее текущей и (при автоматической
/// проверке) её не пропустили.
pub fn should_offer(latest: Version, current: Version, skipped: &str, manual: bool) -> bool {
    latest > current && (manual || Version::parse(skipped) != Some(latest))
}

/// Пора ли автоматически проверять: проверка включена и с прошлой прошло
/// не меньше суток (или часы перевели назад).
pub fn check_due(enabled: bool, last_check: u64, now: u64) -> bool {
    enabled && (last_check == 0 || now < last_check || now - last_check >= CHECK_INTERVAL_SECS)
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

// ------------------------------------------------------------------ сеть

/// Заголовок User-Agent: только название и версия программы.
fn user_agent() -> String {
    format!("sshdeck/{CURRENT}")
}

/// Аргументы `curl`, общие для всех запросов: только HTTPS (на тестовом
/// сервере — и HTTP), ограничения по времени и размеру.
fn curl_base(endpoints: &Endpoints, max_bytes: u64, max_time_secs: u32) -> Command {
    let protocols = if endpoints.allow_http {
        "=https,http"
    } else {
        "=https"
    };
    let mut command = Command::new("curl");
    command
        .args(["-fsSL", "--proto", protocols, "--proto-redir", protocols])
        .args(["--connect-timeout", "20"])
        .arg("--max-time")
        .arg(max_time_secs.to_string())
        .arg("--max-filesize")
        .arg(max_bytes.to_string())
        .arg("-A")
        .arg(user_agent())
        .stdin(Stdio::null());
    command
}

/// Ошибка запуска `curl`.
fn curl_spawn_error(err: &std::io::Error) -> String {
    if err.kind() == std::io::ErrorKind::NotFound {
        tr!("Checking for updates requires curl").to_string()
    } else {
        tr!("Could not run curl: {}", err)
    }
}

/// Понятная причина по коду выхода `curl`.
fn curl_reason(code: Option<i32>, stderr: &str) -> String {
    let detail = stderr.trim().trim_start_matches("curl: ");
    match code {
        Some(6) => {
            tr!("server not found — check the address and your internet connection").to_string()
        }
        Some(7) => tr!("the server is not responding").to_string(),
        Some(28) => tr!("the server is taking too long to respond").to_string(),
        Some(35 | 60) => tr!("secure connection error ({})", detail),
        Some(63) => tr!("the file is larger than expected").to_string(),
        Some(22) if detail.contains("404") => {
            tr!("nothing found at the link (error 404)").to_string()
        }
        Some(22) if detail.contains("403") || detail.contains("429") => {
            tr!("GitHub is limiting requests, try again later").to_string()
        }
        _ => detail.to_string(),
    }
}

/// Последний выпуск. `Ok(None)` — выпусков нет или последний не
/// предлагается (см. [`parse_release`]).
pub fn fetch_latest(endpoints: &Endpoints) -> Result<Option<Release>, String> {
    let output = curl_base(endpoints, MAX_API_RESPONSE, 30)
        .args(["-H", "Accept: application/vnd.github+json"])
        .args(["-H", "X-GitHub-Api-Version: 2022-11-28"])
        .arg("--")
        .arg(&endpoints.api)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| curl_spawn_error(&err))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(22) && stderr.contains("404") {
            return Ok(None);
        }
        return Err(curl_reason(output.status.code(), &stderr));
    }
    if output.stdout.len() as u64 > MAX_API_RESPONSE {
        return Err(tr!("the file is larger than expected").to_string());
    }
    parse_release(&String::from_utf8_lossy(&output.stdout))
}

/// Каталог для скачанных обновлений.
pub fn downloads_dir() -> PathBuf {
    gtk::glib::user_cache_dir().join("sshdeck").join("updates")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).expect(text)
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("0.9.10") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert_eq!(v("v0.5.0"), v("0.5.0"));
        assert!(v("0.5.0") <= v("0.5.0"));
        assert_eq!(v("0.10.2").to_string(), "0.10.2");
        assert_eq!(Version::current().to_string(), CURRENT);
    }

    #[test]
    fn unusual_versions_are_not_offered() {
        for text in [
            "",
            "v",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.3-rc1",
            "1.2.3+git",
            "v1..2",
            "a.b.c",
            "-1.2.3",
            " 1. 2.3",
            "1.2.3 beta",
            "9999999999.0.0",
        ] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
    }

    const FIXTURE: &str = r#"{
      "url": "https://api.github.com/repos/ylkhn1/sshdeck/releases/1",
      "html_url": "https://evil.example/sshdeck/releases/tag/v0.5.0",
      "tag_name": "v0.5.0",
      "name": "sshdeck 0.5.0",
      "draft": false,
      "prerelease": false,
      "assets": [
        {"name": "sshdeck-0.5.0-1.x86_64.rpm", "size": 3516669,
         "browser_download_url": "https://evil.example/a.rpm"},
        {"name": "sshdeck-0.5.0-1.x86_64.rpm.minisig", "size": 330},
        {"name": "sshdeck-0.5.0-x86_64-linux.tar.gz", "size": 4772415},
        {"name": "sshdeck-0.5.0-x86_64-linux.tar.gz.minisig", "size": 340},
        {"name": "sshdeck_0.5.0-1_amd64.deb", "size": 3209208}
      ],
      "body": "New: ..."
    }"#;

    #[test]
    fn release_json_is_parsed() {
        let release = parse_release(FIXTURE).unwrap().unwrap();
        assert_eq!(release.version, v("0.5.0"));
        assert_eq!(release.assets.len(), 5);
        assert_eq!(
            release.asset_size("sshdeck-0.5.0-x86_64-linux.tar.gz"),
            Some(4772415)
        );
    }

    #[test]
    fn drafts_prereleases_and_odd_tags_are_skipped() {
        let draft = FIXTURE.replace(r#""draft": false"#, r#""draft": true"#);
        assert_eq!(parse_release(&draft).unwrap(), None);
        let pre = FIXTURE.replace(r#""prerelease": false"#, r#""prerelease": true"#);
        assert_eq!(parse_release(&pre).unwrap(), None);
        let rc = FIXTURE.replace("\"v0.5.0\"", "\"v0.5.0-rc1\"");
        assert_eq!(parse_release(&rc).unwrap(), None);
        assert!(parse_release("not json").is_err());
        assert!(parse_release(r#"{"message": "Not Found"}"#).is_err());
        // Без списка файлов выпуск всё равно разбирается.
        let bare = parse_release(r#"{"tag_name": "v1.2.3"}"#).unwrap().unwrap();
        assert!(bare.assets.is_empty());
    }

    #[test]
    fn asset_names_per_kind() {
        let version = v("0.10.1");
        assert_eq!(
            InstallKind::Tarball.asset_name(version).unwrap(),
            "sshdeck-0.10.1-x86_64-linux.tar.gz"
        );
        assert_eq!(
            InstallKind::Deb.asset_name(version).unwrap(),
            "sshdeck_0.10.1-1_amd64.deb"
        );
        assert_eq!(
            InstallKind::Rpm.asset_name(version).unwrap(),
            "sshdeck-0.10.1-1.x86_64.rpm"
        );
        assert_eq!(InstallKind::Aur.asset_name(version), None);
        assert_eq!(InstallKind::Unknown.asset_name(version), None);
        assert_eq!(InstallKind::from_code(Some("deb")), InstallKind::Deb);
        assert_eq!(InstallKind::from_code(Some("other")), InstallKind::Unknown);
        assert_eq!(InstallKind::from_code(None), InstallKind::Unknown);
    }

    #[test]
    fn download_urls_are_built_and_checked() {
        let github = Endpoints::github();
        let version = v("0.5.0");
        assert_eq!(
            github.api,
            "https://api.github.com/repos/ylkhn1/sshdeck/releases/latest"
        );
        let url = github.download_url(version, "sshdeck_0.5.0-1_amd64.deb");
        assert_eq!(
            url,
            "https://github.com/ylkhn1/sshdeck/releases/download/v0.5.0/sshdeck_0.5.0-1_amd64.deb"
        );
        assert!(
            github
                .check_download_url(&url, version, InstallKind::Deb)
                .is_ok()
        );
        assert!(
            github
                .check_download_url(&format!("{url}.minisig"), version, InstallKind::Deb)
                .is_ok()
        );
        assert_eq!(
            github.release_page(version),
            "https://github.com/ylkhn1/sshdeck/releases/tag/v0.5.0"
        );
        for bad in [
            // Чужой файл, другая версия, другой способ установки.
            "https://github.com/ylkhn1/sshdeck/releases/download/v0.5.0/other.deb",
            "https://github.com/ylkhn1/sshdeck/releases/download/v0.6.0/sshdeck_0.6.0-1_amd64.deb",
            "https://github.com/ylkhn1/sshdeck/releases/download/v0.5.0/sshdeck-0.5.0-1.x86_64.rpm",
            // Другой репозиторий, хост, протокол, хитрые пути.
            "https://github.com/someone/sshdeck/releases/download/v0.5.0/sshdeck_0.5.0-1_amd64.deb",
            "https://evil.example/ylkhn1/sshdeck/releases/download/v0.5.0/sshdeck_0.5.0-1_amd64.deb",
            "http://github.com/ylkhn1/sshdeck/releases/download/v0.5.0/sshdeck_0.5.0-1_amd64.deb",
            "https://github.com/ylkhn1/sshdeck/releases/download/v0.5.0/../x/sshdeck_0.5.0-1_amd64.deb",
            "https://github.com/ylkhn1/sshdeck/releases/download/v0.5.0/sshdeck_0.5.0-1_amd64.deb?x",
        ] {
            assert!(
                github
                    .check_download_url(bad, version, InstallKind::Deb)
                    .is_err(),
                "{bad}"
            );
        }
        assert!(
            github
                .check_download_url(&url, version, InstallKind::Aur)
                .is_err()
        );
        // Тестовый сервер: HTTP разрешён, но только по тому же шаблону.
        let local = Endpoints::with_bases("http://127.0.0.1:8000", "http://127.0.0.1:8000", true);
        let ok = local.download_url(version, "sshdeck_0.5.0-1_amd64.deb");
        assert!(ok.starts_with("http://127.0.0.1:8000/ylkhn1/sshdeck/releases/download/v0.5.0/"));
        assert!(
            local
                .check_download_url(&ok, version, InstallKind::Deb)
                .is_ok()
        );
        assert!(
            local
                .check_download_url(&url, version, InstallKind::Deb)
                .is_err()
        );
    }

    #[test]
    fn method_depends_on_kind_and_location() {
        let yes = |_: &str| true;
        let no = |_: &str| false;
        let usr = Path::new("/usr/bin/sshdeck");
        // Архивная сборка в /usr себя не трогает.
        assert_eq!(
            method(InstallKind::Tarball, Some(usr), "x86_64", yes),
            Method::ReleasePage
        );
        assert_eq!(
            method(InstallKind::Tarball, None, "x86_64", yes),
            Method::ReleasePage
        );
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("sshdeck");
        std::fs::write(&exe, b"\x7fELF").unwrap();
        assert_eq!(
            method(InstallKind::Tarball, Some(&exe), "x86_64", yes),
            Method::SelfReplace(exe.clone())
        );
        assert_eq!(
            method(InstallKind::Tarball, Some(&exe), "aarch64", yes),
            Method::ReleasePage
        );
        assert_eq!(
            method(InstallKind::Deb, Some(usr), "x86_64", yes),
            Method::Package(InstallKind::Deb)
        );
        assert_eq!(
            method(InstallKind::Rpm, Some(usr), "x86_64", yes),
            Method::Package(InstallKind::Rpm)
        );
        // Без менеджера пакетов или не из /usr/bin — только ссылка.
        assert_eq!(
            method(InstallKind::Deb, Some(usr), "x86_64", no),
            Method::ReleasePage
        );
        assert_eq!(
            method(InstallKind::Rpm, Some(&exe), "x86_64", yes),
            Method::ReleasePage
        );
        assert_eq!(
            method(InstallKind::Aur, Some(usr), "x86_64", yes),
            Method::Aur
        );
        assert_eq!(
            method(InstallKind::Unknown, Some(&exe), "x86_64", yes),
            Method::ReleasePage
        );
    }

    #[test]
    fn offer_needs_the_asset_and_its_signature() {
        let release = parse_release(FIXTURE).unwrap().unwrap();
        let exe = PathBuf::from("/home/u/.local/bin/sshdeck");
        let tarball = offer(&release, Method::SelfReplace(exe.clone()));
        assert_eq!(tarball.method, Method::SelfReplace(exe));
        assert_eq!(
            tarball.asset,
            Some(("sshdeck-0.5.0-x86_64-linux.tar.gz".to_string(), 4772415))
        );
        let rpm = offer(&release, Method::Package(InstallKind::Rpm));
        assert_eq!(rpm.method, Method::Package(InstallKind::Rpm));
        // У .deb в выпуске нет подписи — только страница выпуска.
        let deb = offer(&release, Method::Package(InstallKind::Deb));
        assert_eq!(deb.method, Method::ReleasePage);
        assert_eq!(deb.asset, None);
        let aur = offer(&release, Method::Aur);
        assert_eq!((aur.method, aur.asset), (Method::Aur, None));
    }

    #[test]
    fn offering_respects_skipped_versions() {
        let current = v("0.4.0");
        assert!(should_offer(v("0.5.0"), current, "", false));
        assert!(!should_offer(v("0.4.0"), current, "", false));
        assert!(!should_offer(v("0.3.9"), current, "", true));
        assert!(!should_offer(v("0.5.0"), current, "0.5.0", false));
        // Ручная проверка показывает и пропущенную версию.
        assert!(should_offer(v("0.5.0"), current, "0.5.0", true));
        // Пропуск относится только к своей версии.
        assert!(should_offer(v("0.6.0"), current, "0.5.0", false));
    }

    #[test]
    fn check_is_due_once_a_day() {
        let now = 1_800_000_000;
        assert!(check_due(true, 0, now));
        assert!(!check_due(false, 0, now));
        assert!(!check_due(true, now - 60, now));
        assert!(!check_due(true, now - CHECK_INTERVAL_SECS + 1, now));
        assert!(check_due(true, now - CHECK_INTERVAL_SECS, now));
        assert!(!check_due(false, now - 10 * CHECK_INTERVAL_SECS, now));
        // Часы перевели назад — проверяем, а не ждём годами.
        assert!(check_due(true, now + 3600, now));
    }
}
