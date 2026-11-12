//! Вход без пароля: локальный SSH-ключ и установка его открытой части на
//! сервер.
//!
//! Ключ выбирается так же, как его выбрал бы сам `ssh` для этого хоста:
//! список `IdentityFile` берётся из `ssh -G` (с тем же `-F`, что и у
//! остального приложения), и используется первый ключ, закрытая часть
//! которого существует. Если такого нет, новый ключ создаётся по первому
//! пути `IdentityFile` из настроек хоста, а когда хост ключ не задаёт —
//! в `~/.ssh/id_ed25519`.
//!
//! Ключ ставится через уже авторизованное мастер-соединение вкладки, поэтому
//! пароль приложению не нужен и нигде не проходит через него. После
//! установки выполняется пробный вход с ключом отдельным соединением — так
//! видно, принял ли сервер ключ (он может читать ключи не из
//! `~/.ssh/authorized_keys`). Все функции блокирующие: вызывать их нужно не
//! из потока GTK.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::remote;
use crate::session::Session;
use crate::ssh_config::{self, Resolved};

const SSH_KEYGEN_BIN: &str = "/usr/bin/ssh-keygen";
const SSH_ADD_BIN: &str = "/usr/bin/ssh-add";

/// Ключ, который создаётся, когда хост не задаёт `IdentityFile`.
const NEW_KEY: &str = ".ssh/id_ed25519";

const MARK_ADDED: &str = "SSHDECK_KEY_ADDED";
const MARK_PRESENT: &str = "SSHDECK_KEY_PRESENT";

/// Сценарий для удалённого `sh`: читает одну строку (открытый ключ) со
/// стандартного ввода и дописывает её в `authorized_keys`, если её там ещё
/// нет. В тексте нет одинарных кавычек: он целиком передаётся в `sh -c '…'`.
const INSTALL_SCRIPT: &str = r#"umask 077; d="$HOME/.ssh"; f="$d/authorized_keys"; mkdir -p "$d" && touch "$f" || exit 1; IFS= read -r key || exit 1; if grep -qxF -e "$key" "$f"; then echo SSHDECK_KEY_PRESENT; else if [ -s "$f" ] && [ -n "$(tail -c 1 "$f")" ]; then echo >> "$f"; fi; printf "%s\n" "$key" >> "$f" && echo SSHDECK_KEY_ADDED; fi"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// Ключ дописан в `authorized_keys`.
    Added,
    /// Такой ключ там уже был.
    AlreadyPresent,
}

/// Результат пробного входа с ключом.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginCheck {
    /// ssh вошёл на сервер без пароля.
    Works,
    /// Вход не удался; текст — последняя строка сообщения ssh.
    Fails(String),
    /// Проверить нельзя: ключ защищён парольной фразой и не загружен в
    /// агент, или хост доступен только через `ProxyJump`.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub installed: Installed,
    /// Файл ключа для показа пользователю: открытая часть, а если её нет
    /// рядом (она вычислена из закрытой) — сам закрытый ключ.
    pub public_key: PathBuf,
    /// Ключ был создан этим вызовом.
    pub created: bool,
    pub login: LoginCheck,
}

/// Какой ключ использовать для хоста.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPlan {
    /// Закрытый ключ: существующий или тот, что будет создан.
    pub private: PathBuf,
    /// Ключа ещё нет, его нужно создать.
    pub create: bool,
    /// Путь взят из `IdentityFile` настроек хоста, а не из списка по
    /// умолчанию.
    pub explicit: bool,
    /// Хост доступен через `ProxyJump`: пробный вход может упереться в
    /// пароль промежуточного сервера, поэтому не выполняется.
    proxy_jump: bool,
}

impl KeyPlan {
    /// Файл открытой части.
    pub fn public(&self) -> PathBuf {
        public_path(&self.private)
    }
}

/// Выбирает ключ для `target` по настройкам, которые применит `ssh`.
/// `home` — домашний каталог для `~` и `%d`.
pub fn plan(target: &str, config: Option<&Path>, home: &Path) -> Result<KeyPlan, String> {
    let resolved = ssh_config::resolve(target, config)?;
    // Список по умолчанию зависит от версии OpenSSH, поэтому его тоже
    // спрашиваем у ssh — с пустым конфигом.
    let defaults = ssh_config::resolve(target, Some(Path::new("/dev/null"))).ok();
    Ok(plan_from(&resolved, defaults.as_ref(), home, &|path| path.is_file()))
}

/// Решение без обращения к ssh: `resolved` — вывод `ssh -G` для хоста,
/// `defaults` — для пустого конфига (если удалось получить).
fn plan_from(
    resolved: &Resolved,
    defaults: Option<&Resolved>,
    home: &Path,
    exists: &dyn Fn(&Path) -> bool,
) -> KeyPlan {
    let raw = resolved.all("identityfile");
    let explicit = match defaults {
        Some(defaults) => raw != defaults.all("identityfile"),
        // Без эталона: свои имена — это всё, что не `~/.ssh/id_*`.
        None => raw.iter().any(|file| !file.starts_with("~/.ssh/id_")),
    };
    let tokens = Tokens::new(resolved, home);
    let files: Vec<PathBuf> = raw
        .iter()
        .filter_map(|file| expand_path(file, &tokens))
        .collect();
    let proxy_jump = resolved
        .get("proxyjump")
        .is_some_and(|jump| !jump.eq_ignore_ascii_case("none"));
    let (private, create) = match files.iter().find(|file| exists(file)) {
        Some(file) => (file.clone(), false),
        None => match files.first() {
            Some(first) if explicit => (first.clone(), true),
            _ => (home.join(NEW_KEY), true),
        },
    };
    // Путь по умолчанию «указанным хостом» не считается.
    let explicit = explicit && files.contains(&private);
    KeyPlan {
        private,
        create,
        explicit,
        proxy_jump,
    }
}

/// Значения токенов ssh_config(5), которые встречаются в `IdentityFile`.
struct Tokens {
    home: PathBuf,
    local_user: String,
    local_host: String,
    remote_host: String,
    remote_user: String,
    port: String,
    alias: String,
}

impl Tokens {
    fn new(resolved: &Resolved, home: &Path) -> Self {
        let local_user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_default();
        let local_host = fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|name| name.trim().to_string())
            .unwrap_or_default();
        let get = |key: &str| resolved.get(key).unwrap_or_default().to_string();
        Self {
            home: home.to_path_buf(),
            local_user,
            local_host,
            remote_host: get("hostname"),
            remote_user: get("user"),
            port: get("port"),
            alias: get("host"),
        }
    }
}

/// Раскрывает `~` и токены в пути из `IdentityFile`. `None` — путь с
/// токеном, который здесь не поддерживается (`%C`, `~другой_пользователь`
/// и т. п.): такой ключ пропускается.
fn expand_path(raw: &str, tokens: &Tokens) -> Option<PathBuf> {
    let home = tokens.home.to_string_lossy();
    let raw = if raw == "~" {
        home.to_string()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        format!("{}/{rest}", home.trim_end_matches('/'))
    } else if raw.starts_with('~') {
        return None;
    } else {
        raw.to_string()
    };
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '%' => out.push('%'),
            'd' => out.push_str(&home),
            'u' => out.push_str(&tokens.local_user),
            'L' => out.push_str(tokens.local_host.split('.').next().unwrap_or_default()),
            'l' => out.push_str(&tokens.local_host),
            'h' => out.push_str(&tokens.remote_host),
            'r' => out.push_str(&tokens.remote_user),
            'p' => out.push_str(&tokens.port),
            'n' => out.push_str(&tokens.alias),
            _ => return None,
        }
    }
    let path = PathBuf::from(out);
    // Относительный путь ssh считает от текущего каталога, которого у
    // приложения осмысленно нет; домашний каталог — разумная замена.
    Some(if path.is_absolute() {
        path
    } else {
        tokens.home.join(path)
    })
}

fn public_path(private: &Path) -> PathBuf {
    let mut name = private.as_os_str().to_owned();
    name.push(".pub");
    PathBuf::from(name)
}

/// Создаёт ключ ed25519 без парольной фразы по пути `private` (каталог
/// создаётся с правами 0700); возвращает путь открытой части.
pub fn generate_key(private: &Path) -> Result<PathBuf, String> {
    if let Some(dir) = private.parent()
        && !dir.exists()
    {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    if private.exists() {
        // Существующий закрытый ключ не перезаписываем ни при каких условиях.
        return Err(missing_public_error(private));
    }
    let output = Command::new(SSH_KEYGEN_BIN)
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(private)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| tr!("Could not start {}: {}", SSH_KEYGEN_BIN, err))?;
    if !output.status.success() {
        return Err(tr!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(public_path(private))
}

fn missing_public_error(private: &Path) -> String {
    tr!(
        "{0} already exists, but there is no .pub file next to it. Restore it with: \
         ssh-keygen -y -f {0} > {0}.pub",
        private.display()
    )
}

/// Открытая часть ключа без парольной фразы, вычисленная из закрытой
/// (`ssh-keygen -y`). Для ключа с парольной фразой — `None`: спросить её
/// приложению негде.
fn derive_public(private: &Path) -> Option<String> {
    let output = Command::new(SSH_KEYGEN_BIN)
        .args(["-y", "-P", "", "-f"])
        .arg(private)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?.trim();
    (output.status.success() && !line.is_empty()).then(|| line.to_string())
}

/// Читает открытый ключ и проверяет, что это одна строка привычного вида.
pub fn read_public(path: &Path) -> Result<String, String> {
    let text = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    parse_public(&text).ok_or_else(|| tr!("{}: not an SSH public key", path.display()))
}

fn parse_public(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or_default();
    let mut parts = line.split_whitespace();
    let kind = parts.next().unwrap_or_default();
    let known_kind = ["ssh-", "ecdsa-", "sk-"]
        .iter()
        .any(|prefix| kind.starts_with(prefix));
    (known_kind && parts.next().is_some() && !line.chars().any(char::is_control))
        .then(|| line.to_string())
}

/// Удалённая команда для [`Session::command_argv`].
fn remote_command() -> String {
    remote::sh(INSTALL_SCRIPT)
}

/// Дописывает открытый ключ в `~/.ssh/authorized_keys` на сервере через
/// мастер-соединение сессии.
pub fn install(session: &Session, public_line: &str) -> Result<Installed, String> {
    let output = remote::run(session, &remote_command(), &format!("{public_line}\n"))?;
    parse_reply(&output.stdout, &output.stderr)
}

fn parse_reply(stdout: &str, stderr: &str) -> Result<Installed, String> {
    if stdout.lines().any(|line| line.trim() == MARK_ADDED) {
        Ok(Installed::Added)
    } else if stdout.lines().any(|line| line.trim() == MARK_PRESENT) {
        Ok(Installed::AlreadyPresent)
    } else {
        let detail = stderr.trim();
        Err(if detail.is_empty() {
            tr!("The server did not confirm that the key was installed").to_string()
        } else {
            tr!("Could not install the key on the server: {}", detail)
        })
    }
}

/// Может ли ssh воспользоваться ключом, ничего не спрашивая: ключ без
/// парольной фразы или уже загружен в агент.
fn usable_without_prompt(private: &Path, public_line: &str) -> bool {
    if derive_public(private).is_some() {
        return true;
    }
    let blob = public_line.split_whitespace().nth(1).unwrap_or_default();
    Command::new(SSH_ADD_BIN)
        .arg("-L")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(blob))
        })
}

/// Команда пробного входа: обычный `ssh` к тому же адресу с теми же
/// настройками, но отдельным соединением (мимо мастера), только по ключу и
/// без вопросов.
fn login_check_argv(session: &Session) -> Vec<String> {
    let mut argv = session.base();
    for option in [
        "BatchMode=yes",
        "ControlMaster=no",
        "ControlPath=none",
        "PreferredAuthentications=publickey",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
        "ConnectTimeout=10",
        "RemoteCommand=none",
        "RequestTTY=no",
        "ClearAllForwardings=yes",
    ] {
        argv.push("-o".to_string());
        argv.push(option.to_string());
    }
    argv.extend(["--".to_string(), session.target().to_string(), "true".to_string()]);
    argv
}

fn check_login(session: &Session) -> LoginCheck {
    let argv = login_check_argv(session);
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => LoginCheck::Works,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .unwrap_or_default();
            LoginCheck::Fails(detail.to_string())
        }
        Err(err) => LoginCheck::Fails(tr!("Could not start ssh: {}", err)),
    }
}

/// Полный путь: взять выбранный ключ или создать его, установить на сервер
/// и проверить вход.
pub fn setup(session: &Session, plan: &KeyPlan) -> Result<Outcome, String> {
    let (public_key, line, created) = if plan.create {
        let public = generate_key(&plan.private)?;
        let line = read_public(&public)?;
        (public, line, true)
    } else {
        let public = plan.public();
        if public.is_file() {
            (public.clone(), read_public(&public)?, false)
        } else {
            let line = derive_public(&plan.private)
                .ok_or_else(|| missing_public_error(&plan.private))?;
            (plan.private.clone(), line, false)
        }
    };
    let installed = install(session, &line)?;
    let login = if plan.proxy_jump || !usable_without_prompt(&plan.private, &line) {
        LoginCheck::Skipped
    } else {
        check_login(session)
    };
    Ok(Outcome {
        installed,
        public_key,
        created,
        login,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::session::SSH_BIN;

    const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa a@test";
    const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb b@test";

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o777)
            .unwrap_or(0)
    }

    /// Выполняет удалённый сценарий локально, подменив домашний каталог.
    fn run_script(home: &Path, key: &str) -> String {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(INSTALL_SCRIPT)
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("sh");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(format!("{key}\n").as_bytes())
            .expect("write");
        let output = child.wait_with_output().expect("wait");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn script_has_no_single_quotes() {
        assert!(!INSTALL_SCRIPT.contains('\''));
        assert!(remote_command().starts_with("sh -c '"));
    }

    #[test]
    fn script_adds_key_once() {
        let home = tempfile::tempdir().expect("tempdir");
        assert_eq!(run_script(home.path(), KEY_A), MARK_ADDED);
        assert_eq!(run_script(home.path(), KEY_A), MARK_PRESENT);
        assert_eq!(run_script(home.path(), KEY_B), MARK_ADDED);

        let file = home.path().join(".ssh/authorized_keys");
        let text = fs::read_to_string(&file).expect("read");
        assert_eq!(text, format!("{KEY_A}\n{KEY_B}\n"));
        assert_eq!(mode_of(&file), 0o600);
        assert_eq!(mode_of(&home.path().join(".ssh")), 0o700);
    }

    #[test]
    fn script_keeps_existing_keys_without_trailing_newline() {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = home.path().join(".ssh");
        fs::create_dir(&dir).expect("mkdir");
        fs::write(dir.join("authorized_keys"), KEY_A).expect("write");
        assert_eq!(run_script(home.path(), KEY_B), MARK_ADDED);
        let text = fs::read_to_string(dir.join("authorized_keys")).expect("read");
        assert_eq!(text, format!("{KEY_A}\n{KEY_B}\n"));
    }

    #[test]
    fn replies_are_recognised() {
        assert_eq!(parse_reply("SSHDECK_KEY_ADDED\n", ""), Ok(Installed::Added));
        assert_eq!(
            parse_reply("motd\nSSHDECK_KEY_PRESENT\n", "warning"),
            Ok(Installed::AlreadyPresent)
        );
        assert!(parse_reply("", "Permission denied").is_err());
        assert!(parse_reply("", "").is_err());
    }

    #[test]
    fn public_key_is_validated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = dir.path().join("good.pub");
        fs::write(&good, format!("\n{KEY_A}\n")).expect("write");
        assert_eq!(read_public(&good), Ok(KEY_A.to_string()));

        let bad = dir.path().join("bad.pub");
        fs::write(&bad, "-----BEGIN OPENSSH PRIVATE KEY-----\n").expect("write");
        assert!(read_public(&bad).is_err());
        assert!(read_public(&dir.path().join("missing.pub")).is_err());
    }

    const DEFAULTS: &str = "identityfile ~/.ssh/id_rsa\nidentityfile ~/.ssh/id_ecdsa\n\
                            identityfile ~/.ssh/id_ed25519\n";

    fn resolved(identity: &str) -> Resolved {
        Resolved::parse(&format!(
            "host web\nuser deploy\nhostname 10.0.0.5\nport 2222\n{identity}"
        ))
    }

    #[test]
    fn default_identity_files_pick_the_first_existing_key() {
        let home = Path::new("/home/u");
        let defaults = Resolved::parse(DEFAULTS);
        let host = resolved(DEFAULTS);
        // Ключей нет: создаётся ~/.ssh/id_ed25519.
        let plan = plan_from(&host, Some(&defaults), home, &|_| false);
        assert_eq!(plan.private, home.join(".ssh/id_ed25519"));
        assert!(plan.create && !plan.explicit);
        // Есть id_ecdsa — берётся он, хотя id_ed25519 «лучше»: так решит ssh.
        let ecdsa = home.join(".ssh/id_ecdsa");
        let plan = plan_from(&host, Some(&defaults), home, &|p| p == ecdsa);
        assert_eq!(plan.private, ecdsa);
        assert!(!plan.create && !plan.explicit);
        assert_eq!(plan.public(), home.join(".ssh/id_ecdsa.pub"));
    }

    #[test]
    fn host_identity_file_is_respected() {
        let home = Path::new("/home/u");
        let defaults = Resolved::parse(DEFAULTS);
        let host = resolved(
            "identityfile ~/.ssh/work key\nidentityfile %d/.ssh/k_%h_%r_%p\nidentitiesonly yes\n",
        );
        // Ни одного ключа: создаётся первый из указанных хостом.
        let plan = plan_from(&host, Some(&defaults), home, &|_| false);
        assert_eq!(plan.private, home.join(".ssh/work key"));
        assert!(plan.create && plan.explicit);
        // Второй существует — используется он.
        let second = home.join(".ssh/k_10.0.0.5_deploy_2222");
        let plan = plan_from(&host, Some(&defaults), home, &|p| p == second);
        assert_eq!(plan.private, second);
        assert!(!plan.create && plan.explicit);
        // Без эталонного списка свои имена тоже распознаются.
        let plan = plan_from(&host, None, home, &|_| false);
        assert!(plan.explicit);
        assert!(!plan_from(&resolved(DEFAULTS), None, home, &|_| false).explicit);
        // Явно указанный ключ со стандартным именем — тоже указан хостом.
        let rsa_only = resolved("identityfile ~/.ssh/id_rsa\n");
        let plan = plan_from(&rsa_only, Some(&defaults), home, &|_| false);
        assert_eq!(plan.private, home.join(".ssh/id_rsa"));
        assert!(plan.create && plan.explicit);
    }

    #[test]
    fn identity_paths_are_expanded() {
        let tokens = Tokens::new(&resolved(""), Path::new("/home/u"));
        let tokens = Tokens {
            local_user: "me".to_string(),
            local_host: "laptop.lan".to_string(),
            ..tokens
        };
        let expand = |raw: &str| expand_path(raw, &tokens);
        assert_eq!(expand("~/.ssh/a"), Some(PathBuf::from("/home/u/.ssh/a")));
        assert_eq!(expand("%d/k-%u@%L-%l"), Some(PathBuf::from("/home/u/k-me@laptop-laptop.lan")));
        assert_eq!(expand("/k/%r@%h:%p-%n-100%%"), Some(PathBuf::from("/k/deploy@10.0.0.5:2222-web-100%")));
        assert_eq!(expand("keys/a"), Some(PathBuf::from("/home/u/keys/a")));
        assert_eq!(expand("/k/%C"), None);
        assert_eq!(expand("/k/%"), None);
        assert_eq!(expand("~root/.ssh/a"), None);
    }

    #[test]
    fn proxy_jump_disables_the_login_check() {
        let home = Path::new("/home/u");
        assert!(!plan_from(&resolved(DEFAULTS), None, home, &|_| false).proxy_jump);
        let jump = resolved("proxyjump bastion\n");
        assert!(plan_from(&jump, None, home, &|_| false).proxy_jump);
    }

    #[test]
    fn login_check_bypasses_the_master() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = Session::new("web", dir.path(), 1, Some(PathBuf::from("/tmp/cfg")));
        let argv = login_check_argv(&session);
        assert_eq!(&argv[..3], [SSH_BIN, "-F", "/tmp/cfg"]);
        assert!(argv.contains(&"ControlPath=none".to_string()));
        assert!(argv.contains(&"BatchMode=yes".to_string()));
        assert_eq!(&argv[argv.len() - 3..], ["--", "web", "true"]);
    }

    #[test]
    fn plan_uses_ssh_config_with_fake_home() {
        if !Path::new(SSH_BIN).exists() {
            return;
        }
        let home = tempfile::tempdir().expect("tempdir");
        let config = home.path().join("config");
        fs::write(
            &config,
            "Host box\n  HostName 192.0.2.7\n  User admin\n  IdentityFile ~/.ssh/box_key\n  IdentitiesOnly yes\n",
        )
        .expect("write");
        let boxed = plan("box", Some(&config), home.path()).expect("plan");
        assert_eq!(boxed.private, home.path().join(".ssh/box_key"));
        assert!(boxed.create && boxed.explicit);
        let other = plan("other", Some(&config), home.path()).expect("plan");
        assert_eq!(other.private, home.path().join(".ssh/id_ed25519"));
        assert!(other.create && !other.explicit);
    }

    #[test]
    fn key_is_generated_and_never_overwritten() {
        let home = tempfile::tempdir().expect("tempdir");
        let ssh_dir = home.path().join(".ssh");
        let private = ssh_dir.join("box_key");
        let public = generate_key(&private).expect("generate");
        assert_eq!(public, ssh_dir.join("box_key.pub"));
        let line = read_public(&public).expect("public");
        assert_eq!(mode_of(&ssh_dir), 0o700);
        assert_eq!(mode_of(&private), 0o600);
        // Открытая часть вычисляется из закрытой, если .pub потерян.
        let derived = derive_public(&private).expect("derive");
        assert_eq!(derived.split_whitespace().nth(1), line.split_whitespace().nth(1));

        // Закрытый ключ без .pub не затирается.
        fs::remove_file(&public).expect("remove pub");
        let before = fs::read(&private).expect("read");
        assert!(generate_key(&private).is_err());
        assert_eq!(fs::read(&private).expect("read"), before);
    }
}
