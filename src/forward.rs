//! Проброс портов через мастер-соединение вкладки (`ssh -O forward`).
//!
//! Три вида, как у ssh: локальный (`-L [адрес:]порт:хост:порт` — порт на
//! этой машине ведёт к хосту, видимому с сервера), удалённый (`-R` — порт на
//! сервере ведёт к хосту, видимому отсюда) и динамический (`-D [адрес:]порт`
//! — SOCKS-прокси на этой машине). Пробросы записываются так же, как в
//! командной строке ssh: `-L 8080:localhost:80`, `-D 1080`; в таком виде они
//! лежат и в `connections.ini` (см. `crate::connections`).
//!
//! Проброс добавляется в общий мастер вкладок с тем же адресом (см.
//! `crate::session`), поэтому он один на все такие вкладки и исчезает вместе
//! с мастером. Повторный `-O forward` того же проброса мастер принимает
//! молча, а `-O cancel` снимает его сразу для всех. Поэтому в процессе
//! ведётся счётчик: сколько вкладок попросили каждый проброс в каждом
//! мастере ([`acquire`], [`release`]); снимать его нужно, только когда
//! счётчик дошёл до нуля.
//!
//! [`Tracker`] — пробросы одной вкладки и их состояние: какие ждут мастера,
//! какие работают, какие не удалось поднять. Когда мастер меняется (разрыв
//! связи и новый вход — новый pid в ответе `ssh -O check`), все пробросы
//! вкладки ставятся заново.
//!
//! Команды идут через [`Session::mux_argv`] (без ssh-конфига), иначе
//! `-O cancel` снял бы и `LocalForward` из конфига хоста. Функции, которые
//! запускают ssh, блокирующие.

use std::collections::HashMap;
use std::fmt;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};

use crate::session::Session;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Local,
    Remote,
    Dynamic,
}

impl Kind {
    /// В порядке выбора в диалоге.
    pub const ALL: [Kind; 3] = [Kind::Local, Kind::Remote, Kind::Dynamic];

    /// Ключ ssh.
    pub fn flag(self) -> &'static str {
        match self {
            Kind::Local => "-L",
            Kind::Remote => "-R",
            Kind::Dynamic => "-D",
        }
    }

    /// Короткое название для людей.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Local => tr!("Local"),
            Kind::Remote => tr!("Remote"),
            Kind::Dynamic => tr!("Dynamic (SOCKS)"),
        }
    }

    /// Слушает ли порт на этой машине (а не на сервере).
    pub fn listens_here(self) -> bool {
        self != Kind::Remote
    }
}

/// Один проброс. Поля хранятся так, как их записал пользователь (после
/// проверки): мастер сравнивает пробросы по тексту, и отменить можно только
/// точно такой же.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Forward {
    pub kind: Kind,
    /// Адрес, на котором слушает порт: `*`, `localhost`, `[::1]`… Без него —
    /// как решит ssh (обычно только localhost).
    pub bind: Option<String>,
    pub port: u16,
    /// Куда ведёт проброс; у динамического — никуда.
    pub dest: Option<(String, u16)>,
}

impl Forward {
    /// Проброс из полей диалога. Пустой адрес — по умолчанию; пустой хост
    /// назначения — ошибка (в диалоге он заполнен `localhost`).
    pub fn from_fields(kind: Kind, bind: &str, port: &str, host: &str, host_port: &str) -> Result<Self, String> {
        let bind = bind.trim();
        let bind = if bind.is_empty() {
            None
        } else {
            check_address(bind, true)?;
            Some(bind.to_string())
        };
        let port = parse_port(port, tr!("Enter the port to listen on (1–65535)"))?;
        let dest = match kind {
            Kind::Dynamic => None,
            Kind::Local | Kind::Remote => {
                let host = host.trim();
                if host.is_empty() {
                    return Err(tr!("Enter the destination host, for example localhost").to_string());
                }
                check_address(host, false)?;
                let host_port = parse_port(host_port, tr!("Enter the destination port (1–65535)"))?;
                Some((host.to_string(), host_port))
            }
        };
        Ok(Self { kind, bind, port, dest })
    }

    /// Разбирает запись вида `-L 8080:localhost:80`, `L 8080:localhost:80`,
    /// `-L8080:localhost:80` или `-D [::1]:1080`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let invalid = || {
            tr!(
                "“{}” is not a valid forward: use -L [address:]port:host:port, \
                 -R [address:]port:host:port or -D [address:]port",
                text
            )
        };
        let body = text.strip_prefix('-').unwrap_or(text);
        let mut chars = body.chars();
        let kind = match chars.next().map(|c| c.to_ascii_uppercase()) {
            Some('L') => Kind::Local,
            Some('R') => Kind::Remote,
            Some('D') => Kind::Dynamic,
            _ => return Err(invalid()),
        };
        let spec = chars.as_str().trim();
        let parts = split_spec(spec).ok_or_else(invalid)?;
        let (bind, rest) = match (kind, parts.len()) {
            (Kind::Dynamic, 1) | (Kind::Local | Kind::Remote, 3) => ("", &parts[..]),
            (Kind::Dynamic, 2) | (Kind::Local | Kind::Remote, 4) if !parts[0].is_empty() => {
                (parts[0], &parts[1..])
            }
            _ => return Err(invalid()),
        };
        if rest.iter().any(|part| part.trim().is_empty()) {
            return Err(invalid());
        }
        match rest {
            [port] => Self::from_fields(kind, bind, port, "", ""),
            [port, host, host_port] => Self::from_fields(kind, bind, port, host, host_port),
            _ => Err(invalid()),
        }
    }

    /// Аргумент ssh: `[адрес:]порт[:хост:порт]`.
    pub fn spec(&self) -> String {
        let mut spec = String::new();
        if let Some(bind) = &self.bind {
            spec.push_str(bind);
            spec.push(':');
        }
        spec.push_str(&self.port.to_string());
        if let Some((host, port)) = &self.dest {
            spec.push_str(&format!(":{host}:{port}"));
        }
        spec
    }

    /// Где слушает порт: `localhost:8080`, `*:8080`, `[::1]:8080`.
    pub fn listen(&self) -> String {
        format!("{}:{}", self.bind.as_deref().unwrap_or("localhost"), self.port)
    }

    /// Куда ведёт: `db:5432`.
    pub fn destination(&self) -> Option<String> {
        self.dest.as_ref().map(|(host, port)| format!("{host}:{port}"))
    }

    /// Коротко для списка: `localhost:8080 → db:5432`.
    pub fn summary(&self) -> String {
        match self.destination() {
            Some(dest) => format!("{} → {dest}", self.listen()),
            None => self.listen(),
        }
    }

    /// Адрес, по которому к пробросу подключаются с этой машины
    /// (`localhost:8080`); у удалённого проброса его нет.
    pub fn local_address(&self) -> Option<String> {
        if !self.kind.listens_here() {
            return None;
        }
        let host = match self.bind.as_deref() {
            None | Some("*" | "localhost" | "0.0.0.0" | "[::]") => "localhost",
            Some(bind) => bind,
        };
        Some(format!("{host}:{}", self.port))
    }

    /// Ссылка для браузера (только у локального проброса).
    pub fn browser_url(&self) -> Option<String> {
        match self.kind {
            Kind::Local => self.local_address().map(|address| format!("http://{address}/")),
            Kind::Remote | Kind::Dynamic => None,
        }
    }

    /// Займут ли два разных проброса один и тот же порт.
    pub fn conflicts_with(&self, other: &Forward) -> bool {
        self != other
            && self.kind.listens_here() == other.kind.listens_here()
            && self.port == other.port
            && self.bind == other.bind
    }
}

/// Запись для файла и поля ввода: `-L 8080:localhost:80`.
impl fmt::Display for Forward {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.kind.flag(), self.spec())
    }
}

/// Порт 1–65535; `empty` — сообщение для пустого поля.
fn parse_port(text: &str, empty: &str) -> Result<u16, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(empty.to_string());
    }
    match text.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(tr!("Invalid port “{}” (must be a number from 1 to 65535)", text)),
    }
}

/// Имя хоста, адрес IPv4 или адрес IPv6 в квадратных скобках; `*` — только
/// как адрес, на котором слушать.
fn check_address(text: &str, bind: bool) -> Result<(), String> {
    let invalid = || tr!("“{}” is not a valid host name or address", text);
    if text == "*" {
        return if bind {
            Ok(())
        } else {
            Err(tr!("“*” can only be used as the address to listen on").to_string())
        };
    }
    if let Some(inner) = text.strip_prefix('[') {
        let inner = inner.strip_suffix(']').ok_or_else(invalid)?;
        let valid = inner.contains(':')
            && inner
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '%'));
        return if valid { Ok(()) } else { Err(invalid()) };
    }
    if text.contains(':') {
        return Err(tr!(
            "Write the IPv6 address “{}” in square brackets: [{}]",
            text,
            text
        ));
    }
    let valid = !text.starts_with(['-', '.'])
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if valid { Ok(()) } else { Err(invalid()) }
}

/// Делит запись по `:`, не трогая двоеточия внутри `[…]`. `None` — скобки
/// не сходятся.
fn split_spec(spec: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut inside = false;
    for (index, c) in spec.char_indices() {
        match c {
            '[' if !inside => inside = true,
            ']' if inside => inside = false,
            '[' | ']' => return None,
            ':' if !inside => {
                parts.push(&spec[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if inside {
        return None;
    }
    parts.push(&spec[start..]);
    Some(parts)
}

/// Список пробросов, записанный через запятую, точку с запятой или с новой
/// строки (поле в диалоге сохранённого подключения).
pub fn parse_list(text: &str) -> Result<Vec<Forward>, String> {
    let mut list: Vec<Forward> = Vec::new();
    for item in text.split([',', ';', '\n']).map(str::trim).filter(|item| !item.is_empty()) {
        let forward = Forward::parse(item)?;
        if list.contains(&forward) {
            return Err(tr!("“{}” is listed twice", forward));
        }
        if let Some(other) = list.iter().find(|other| other.conflicts_with(&forward)) {
            return Err(tr!("“{}” and “{}” use the same port", other, forward));
        }
        list.push(forward);
    }
    Ok(list)
}

/// Обратное к [`parse_list`].
pub fn format_list(list: &[Forward]) -> String {
    list.iter().map(Forward::to_string).collect::<Vec<_>>().join(", ")
}

// ------------------------------------------------------ счётчик вкладок

/// Сколько вкладок процесса попросили каждый проброс в каждом мастере.
static USERS: LazyLock<Mutex<HashMap<(PathBuf, Forward), usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Ещё одна вкладка просит проброс в мастере `control_path`. Возвращает
/// число вкладок вместе с этой.
pub fn acquire(control_path: &Path, forward: &Forward) -> usize {
    let mut users = USERS.lock().unwrap_or_else(|e| e.into_inner());
    let count = users.entry((control_path.to_path_buf(), forward.clone())).or_insert(0);
    *count += 1;
    *count
}

/// Вкладка больше не нуждается в пробросе; `true`, если она была последней
/// и проброс пора снять в мастере.
pub fn release(control_path: &Path, forward: &Forward) -> bool {
    let mut users = USERS.lock().unwrap_or_else(|e| e.into_inner());
    let key = (control_path.to_path_buf(), forward.clone());
    match users.get_mut(&key) {
        Some(count) if *count > 1 => {
            *count -= 1;
            false
        }
        Some(_) => {
            users.remove(&key);
            true
        }
        None => false,
    }
}

/// Сколько вкладок сейчас держат проброс.
pub fn users(control_path: &Path, forward: &Forward) -> usize {
    let users = USERS.lock().unwrap_or_else(|e| e.into_inner());
    users
        .get(&(control_path.to_path_buf(), forward.clone()))
        .copied()
        .unwrap_or(0)
}

// ------------------------------------------------- пробросы одной вкладки

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Мастера ещё (или уже) нет; проброс поставится, когда он появится.
    Waiting,
    /// Команда отправлена мастеру.
    Applying,
    Active,
    /// Мастер отказал; текст — объяснение ssh.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub forward: Forward,
    pub state: State,
    /// Записан ли проброс в сохранённое подключение вкладки.
    pub saved: bool,
}

/// Проброс, который пора поставить, и поколение мастера, для которого он
/// ставится (ответ для старого мастера не учитывается).
pub type Job = (Forward, u64);

/// Пробросы вкладки и их состояние. Каждый проброс учитывается в общем
/// счётчике ([`acquire`]); при уничтожении всё отпускается.
#[derive(Debug)]
pub struct Tracker {
    control_path: PathBuf,
    entries: Vec<Entry>,
    /// Pid мастера, которому отправлены пробросы.
    master: Option<u32>,
    generation: u64,
}

impl Tracker {
    pub fn new(control_path: &Path) -> Self {
        Self {
            control_path: control_path.to_path_buf(),
            entries: Vec::new(),
            master: None,
            generation: 0,
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Известен ли живой мастер.
    pub fn has_master(&self) -> bool {
        self.master.is_some()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Работающие пробросы.
    #[cfg(test)]
    pub fn active(&self) -> impl Iterator<Item = &Forward> {
        self.entries
            .iter()
            .filter(|entry| entry.state == State::Active)
            .map(|entry| &entry.forward)
    }

    /// Добавляет проброс. Если мастер уже есть, возвращает задание
    /// поставить его.
    pub fn add(&mut self, forward: Forward, saved: bool) -> Result<Vec<Job>, String> {
        if self.entries.iter().any(|entry| entry.forward == forward) {
            return Err(tr!("This forward is already in the list").to_string());
        }
        if let Some(other) = self.entries.iter().find(|entry| entry.forward.conflicts_with(&forward)) {
            return Err(tr!(
                "Port {} is already used by another forward in this tab: {}",
                forward.listen(),
                other.forward.summary()
            ));
        }
        acquire(&self.control_path, &forward);
        self.entries.push(Entry {
            forward,
            state: State::Waiting,
            saved,
        });
        Ok(self.pending())
    }

    /// Убирает проброс. `Some(true)` — вкладка была последней, кто его
    /// просил, и его нужно снять в мастере.
    pub fn remove(&mut self, forward: &Forward) -> Option<bool> {
        let index = self.entries.iter().position(|entry| &entry.forward == forward)?;
        self.entries.remove(index);
        Some(release(&self.control_path, forward))
    }

    /// Отмечает, записан ли проброс в сохранённое подключение.
    pub fn set_saved(&mut self, forward: &Forward, saved: bool) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| &entry.forward == forward) {
            entry.saved = saved;
        }
    }

    /// Очередной ответ `ssh -O check`: pid мастера или `None`, если его нет.
    /// С новым мастером все пробросы ставятся заново; возвращаются задания.
    pub fn master(&mut self, pid: Option<u32>) -> Vec<Job> {
        if pid != self.master {
            self.master = pid;
            self.generation += 1;
            for entry in &mut self.entries {
                entry.state = State::Waiting;
            }
        }
        self.pending()
    }

    /// Ждущие пробросы, если мастер есть: они переходят в «ставится».
    fn pending(&mut self) -> Vec<Job> {
        if self.master.is_none() {
            return Vec::new();
        }
        let generation = self.generation;
        self.entries
            .iter_mut()
            .filter(|entry| entry.state == State::Waiting)
            .map(|entry| {
                entry.state = State::Applying;
                (entry.forward.clone(), generation)
            })
            .collect()
    }

    /// Ждёт ли проброс ответа для этого поколения мастера.
    pub fn is_applying(&self, forward: &Forward, generation: u64) -> bool {
        generation == self.generation
            && self
                .entries
                .iter()
                .any(|entry| &entry.forward == forward && entry.state == State::Applying)
    }

    /// Результат задания. `false`, если он устарел (мастер сменился или
    /// проброс убрали) и ничего не изменилось.
    pub fn finish(&mut self, forward: &Forward, generation: u64, result: Result<(), String>) -> bool {
        if !self.is_applying(forward, generation) {
            return false;
        }
        if let Some(entry) = self.entries.iter_mut().find(|entry| &entry.forward == forward) {
            entry.state = match result {
                Ok(()) => State::Active,
                Err(message) => State::Failed(message),
            };
        }
        true
    }

    /// Ещё одна попытка для проброса, который не удалось поставить.
    pub fn retry(&mut self, forward: &Forward) -> Vec<Job> {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| &entry.forward == forward && matches!(entry.state, State::Failed(_)))
        {
            entry.state = State::Waiting;
        }
        self.pending()
    }

    /// Отпускает все пробросы (вкладка закрывается). Возвращает те, которые
    /// больше никому не нужны и которые нужно снять в мастере.
    pub fn release_all(&mut self) -> Vec<Forward> {
        self.entries
            .drain(..)
            .filter(|entry| release(&self.control_path, &entry.forward))
            .map(|entry| entry.forward)
            .collect()
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        self.release_all();
    }
}

// ------------------------------------------------------- команды ssh

/// `ssh -F /dev/null -S <сокет> -L <spec> -O <op> -- <хост>`; `op` —
/// `forward` или `cancel`.
pub fn argv(session: &Session, op: &str, forward: &Forward) -> Vec<String> {
    session.mux_argv(op, &[forward.kind.flag(), &forward.spec()])
}

/// Pid мастера из ответа `ssh -O check`: `Master running (pid=1234)`.
pub fn parse_master_pid(text: &str) -> Option<u32> {
    let start = text.find("(pid=")? + "(pid=".len();
    let rest = &text[start..];
    rest[..rest.find(')')?].trim().parse().ok()
}

/// Pid живого мастера сессии; `None`, если мастера нет.
pub fn master_pid(session: &Session) -> Option<u32> {
    let output = output(&session.check_argv()).ok()?;
    if !output.0 {
        return None;
    }
    // Старые версии ssh могли не писать pid — тогда считаем его нулём.
    Some(parse_master_pid(&output.1).unwrap_or(0))
}

/// Успех и stderr команды.
fn output(argv: &[String]) -> Result<(bool, String), String> {
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| tr!("Could not start ssh: {}", err))?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// Объяснение ssh без служебных префиксов: из
/// `mux_client_forward: forwarding request failed: Port forwarding failed`
/// и `muxclient: master forward request failed` остаётся
/// `Port forwarding failed`.
pub fn ssh_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("muxclient:"))
        .map(|line| match line.find("request failed: ") {
            Some(at) => &line[at + "request failed: ".len()..],
            None => line,
        })
        .collect();
    if lines.is_empty() {
        tr!("ssh did not explain the error").to_string()
    } else {
        lines.join("; ")
    }
}

/// Почему порт нельзя занять на этой машине.
fn local_bind_problem(forward: &Forward) -> Option<String> {
    let host = match forward.bind.as_deref() {
        None | Some("localhost") => "127.0.0.1",
        Some("*") => "0.0.0.0",
        Some(bind) => bind.trim_start_matches('[').trim_end_matches(']'),
    };
    let err = TcpListener::bind((host, forward.port)).err()?;
    Some(match err.kind() {
        std::io::ErrorKind::AddrInUse => tr!("the port is already in use").to_string(),
        std::io::ErrorKind::PermissionDenied => {
            tr!("ports below 1024 can only be opened by the administrator").to_string()
        }
        _ => err.to_string(),
    })
}

/// Ставит проброс в мастер сессии.
///
/// Порт на этой машине сначала проверяется: ssh слушает `localhost` и по
/// IPv4, и по IPv6 и считает проброс удачным, если удалось хотя бы одно, —
/// порт, занятый другой программой на 127.0.0.1, остался бы занят ею. Сам
/// ssh при отказе пишет только «Port forwarding failed». Проверка не нужна,
/// если проброс просила и другая вкладка: тогда порт может держать сам
/// мастер.
pub fn apply(session: &Session, forward: &Forward) -> Result<(), String> {
    let shared = users(session.control_path(), forward) > 1;
    if forward.kind.listens_here()
        && !shared
        && let Some(problem) = local_bind_problem(forward)
    {
        return Err(tr!("{}: {}", forward.listen(), problem));
    }
    let (ok, stderr) = output(&argv(session, "forward", forward))?;
    if ok { Ok(()) } else { Err(ssh_error(&stderr)) }
}

/// Снимает проброс в мастере. ssh завершается успешно, даже если такого
/// проброса не было, поэтому ошибка определяется по тексту.
pub fn cancel(session: &Session, forward: &Forward) -> Result<(), String> {
    let (ok, stderr) = output(&argv(session, "cancel", forward))?;
    if ok && !stderr.contains("failed") {
        Ok(())
    } else {
        Err(ssh_error(&stderr))
    }
}

#[cfg(test)]
mod live_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn fwd(text: &str) -> Forward {
        Forward::parse(text).unwrap_or_else(|err| panic!("{text}: {err}"))
    }

    #[test]
    fn parses_ssh_style_records() {
        let local = fwd("-L 8080:localhost:80");
        assert_eq!(local.kind, Kind::Local);
        assert_eq!(local.bind, None);
        assert_eq!(local.port, 8080);
        assert_eq!(local.dest, Some(("localhost".to_string(), 80)));
        assert_eq!(fwd("L 8080:localhost:80"), local);
        assert_eq!(fwd("-L8080:localhost:80"), local);
        assert_eq!(fwd("  l  8080:localhost:80 "), local);

        let remote = fwd("-R 0.0.0.0:9000:127.0.0.1:3000");
        assert_eq!(remote.kind, Kind::Remote);
        assert_eq!(remote.bind.as_deref(), Some("0.0.0.0"));
        assert_eq!(remote.dest, Some(("127.0.0.1".to_string(), 3000)));

        let socks = fwd("-D 1080");
        assert_eq!((socks.kind, socks.port, socks.dest.clone()), (Kind::Dynamic, 1080, None));
        assert_eq!(fwd("-D *:1080").bind.as_deref(), Some("*"));

        let v6 = fwd("-L [::1]:8080:[fe80::1%eth0]:80");
        assert_eq!(v6.bind.as_deref(), Some("[::1]"));
        assert_eq!(v6.dest, Some(("[fe80::1%eth0]".to_string(), 80)));
    }

    #[test]
    fn formats_back_to_the_same_record() {
        for text in [
            "-L 8080:localhost:80",
            "-L [::1]:8080:db.internal:5432",
            "-R *:9000:localhost:3000",
            "-D 1080",
            "-D 127.0.0.1:1080",
        ] {
            let forward = fwd(text);
            assert_eq!(forward.to_string(), text);
            assert_eq!(fwd(&forward.to_string()), forward);
        }
        assert_eq!(fwd("-L [::1]:8080:db:5432").spec(), "[::1]:8080:db:5432");
    }

    #[test]
    fn rejects_broken_records() {
        for text in [
            "",
            "-X 8080:a:80",
            "-L",
            "-L 8080",
            "-L 8080:host",
            "-L a:b:c:d:e",
            "-L :8080:host:80",
            "-L 8080::80",
            "-D 1080:host:80",
            "-L [::1:8080:host:80",
            "-L ::1:8080:host:80",
            "-L 0:host:80",
            "-L 70000:host:80",
            "-L 8080:host:0",
            "-L 8080:-oProxy:80",
            "-L 8080:*:80",
            "-L 8080:a b:80",
            "-L 8080:/tmp/sock:80",
        ] {
            assert!(Forward::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn validates_dialog_fields_with_clear_messages() {
        let ok = Forward::from_fields(Kind::Local, " ", " 8080 ", " localhost ", "80").expect("ok");
        assert_eq!(ok.to_string(), "-L 8080:localhost:80");
        let socks = Forward::from_fields(Kind::Dynamic, "", "1080", "ignored host", "").expect("ok");
        assert_eq!(socks.to_string(), "-D 1080");

        let err = |kind, bind, port, host, host_port| {
            Forward::from_fields(kind, bind, port, host, host_port).expect_err("must fail")
        };
        assert!(err(Kind::Local, "", "", "localhost", "80").contains("1–65535"));
        assert!(err(Kind::Local, "", "99999", "localhost", "80").contains("99999"));
        assert!(err(Kind::Local, "", "8080", "", "80").contains("localhost"));
        assert!(err(Kind::Remote, "", "8080", "localhost", "").contains("destination port"));
        let v6 = err(Kind::Local, "::1", "8080", "localhost", "80");
        assert!(v6.contains("[::1]"), "{v6}");
        let v6 = err(Kind::Local, "", "8080", "fe80::1", "80");
        assert!(v6.contains("[fe80::1]"), "{v6}");
        assert!(err(Kind::Local, "", "8080", "*", "80").contains('*'));
        assert!(err(Kind::Local, "[]", "8080", "h", "80").contains("[]"));
        assert!(err(Kind::Local, "[1.2.3.4]", "8080", "h", "80").contains("[1.2.3.4]"));
    }

    #[test]
    fn addresses_for_people() {
        let local = fwd("-L 8080:db:5432");
        assert_eq!(local.summary(), "localhost:8080 → db:5432");
        assert_eq!(local.local_address().as_deref(), Some("localhost:8080"));
        assert_eq!(local.browser_url().as_deref(), Some("http://localhost:8080/"));
        assert_eq!(fwd("-L *:8080:db:80").local_address().as_deref(), Some("localhost:8080"));
        assert_eq!(fwd("-L [::1]:8080:db:80").browser_url().as_deref(), Some("http://[::1]:8080/"));

        let socks = fwd("-D 1080");
        assert_eq!(socks.summary(), "localhost:1080");
        assert_eq!(socks.local_address().as_deref(), Some("localhost:1080"));
        assert_eq!(socks.browser_url(), None);

        let remote = fwd("-R 9000:localhost:3000");
        assert_eq!(remote.local_address(), None);
        assert_eq!(remote.browser_url(), None);
    }

    #[test]
    fn conflicts_are_about_the_listening_side() {
        let a = fwd("-L 8080:a:80");
        assert!(a.conflicts_with(&fwd("-L 8080:b:80")));
        assert!(a.conflicts_with(&fwd("-D 8080")));
        assert!(!a.conflicts_with(&fwd("-R 8080:a:80")));
        assert!(!a.conflicts_with(&fwd("-L 127.0.0.2:8080:a:80")));
        assert!(!a.conflicts_with(&a.clone()));
    }

    #[test]
    fn lists_round_trip_and_reject_duplicates() {
        let list = parse_list("-L 8080:localhost:80, -D 1080;\n R 9000:localhost:3000,,").expect("list");
        assert_eq!(list.len(), 3);
        assert_eq!(format_list(&list), "-L 8080:localhost:80, -D 1080, -R 9000:localhost:3000");
        assert_eq!(parse_list(&format_list(&list)), Ok(list));
        assert_eq!(parse_list("  "), Ok(Vec::new()));
        assert!(parse_list("-D 1080, -D 1080").is_err());
        assert!(parse_list("-D 1080, -L 1080:a:1").is_err());
        assert!(parse_list("-D 1080, nonsense").is_err());
    }

    #[test]
    fn commands_go_to_the_master_without_the_config() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let session = Session::new("user@example", dir.path(), 41, Some(PathBuf::from("/tmp/cfg")));
        let argv = argv(&session, "forward", &fwd("-L [::1]:8080:db:5432"));
        let sock = session.control_path().to_string_lossy().into_owned();
        assert_eq!(
            argv,
            [
                crate::session::SSH_BIN,
                "-F",
                "/dev/null",
                "-S",
                &sock,
                "-L",
                "[::1]:8080:db:5432",
                "-O",
                "forward",
                "--",
                "user@example",
            ]
        );
        let cancel = super::argv(&session, "cancel", &fwd("-D 1080"));
        assert_eq!(&cancel[cancel.len() - 6..], ["-D", "1080", "-O", "cancel", "--", "user@example"]);
    }

    #[test]
    fn ssh_output_is_cleaned_up() {
        assert_eq!(parse_master_pid("Master running (pid=3562576)\n"), Some(3562576));
        assert_eq!(parse_master_pid("Control socket connect(/x): No such file"), None);
        assert_eq!(
            ssh_error(
                "mux_client_forward: forwarding request failed: Port forwarding failed\n\
                 muxclient: master forward request failed\n"
            ),
            "Port forwarding failed"
        );
        assert_eq!(
            ssh_error(
                "mux_client_forward: forwarding request failed: remote port forwarding failed for listen port 22\n\
                 muxclient: master forward request failed\n"
            ),
            "remote port forwarding failed for listen port 22"
        );
        assert_eq!(
            ssh_error("Control socket connect(/run/x.sock): No such file or directory\n"),
            "Control socket connect(/run/x.sock): No such file or directory"
        );
        assert!(!ssh_error("\n").is_empty());
    }

    #[test]
    fn busy_local_port_is_explained() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let forward = Forward::from_fields(Kind::Local, "", &port.to_string(), "h", "80").expect("fwd");
        assert!(local_bind_problem(&forward).is_some());
        drop(listener);
        assert_eq!(local_bind_problem(&forward), None);
    }

    #[test]
    fn identical_forwards_are_counted_per_master() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let master = dir.path().join("m1.sock");
        let other = dir.path().join("m2.sock");
        let f = fwd("-L 8080:localhost:80");
        assert_eq!(acquire(&master, &f), 1);
        assert_eq!(acquire(&master, &f), 2);
        assert_eq!(acquire(&other, &f), 1);
        assert_eq!(users(&master, &fwd("-L 8080:localhost:81")), 0);
        assert!(!release(&master, &f), "вторая вкладка ещё пользуется");
        assert!(release(&master, &f), "последняя вкладка снимает проброс");
        assert!(!release(&master, &f), "лишнее отпускание ничего не делает");
        assert_eq!(users(&master, &f), 0);
        assert_eq!(users(&other, &f), 1);
        assert!(release(&other, &f));
    }

    #[test]
    fn tabs_share_forwards_and_the_last_one_cancels() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let master = dir.path().join("m.sock");
        let f = fwd("-L 8080:localhost:80");
        let g = fwd("-D 1080");
        let mut first = Tracker::new(&master);
        let mut second = Tracker::new(&master);
        first.add(f.clone(), false).expect("add");
        first.add(g.clone(), false).expect("add");
        second.add(f.clone(), true).expect("add");
        assert_eq!(users(&master, &f), 2);
        assert!(second.entries()[0].saved);

        // Первая вкладка убирает проброс — вторая им ещё пользуется.
        assert_eq!(first.remove(&f), Some(false));
        assert_eq!(first.remove(&f), None);
        // Закрытие первой вкладки: её собственный -D больше никому не нужен.
        assert_eq!(first.release_all(), vec![g.clone()]);
        // Закрытие второй — последней с -L.
        drop(second);
        assert_eq!(users(&master, &f), 0);
        assert_eq!(users(&master, &g), 0);
    }

    #[test]
    fn duplicates_and_port_clashes_in_one_tab_are_refused() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let mut tracker = Tracker::new(&dir.path().join("m.sock"));
        tracker.add(fwd("-L 8080:a:80"), false).expect("add");
        assert!(tracker.add(fwd("-L 8080:a:80"), false).is_err());
        let clash = tracker.add(fwd("-D 8080"), false).expect_err("clash");
        assert!(clash.contains("localhost:8080"), "{clash}");
        assert!(tracker.add(fwd("-R 8080:a:80"), false).is_ok());
    }

    #[test]
    fn forwards_wait_for_the_master_and_come_back_after_reconnect() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let mut tracker = Tracker::new(&dir.path().join("m.sock"));
        let f = fwd("-L 8080:a:80");
        let g = fwd("-R 9000:localhost:3000");
        // До входа проброс только ждёт.
        assert_eq!(tracker.add(f.clone(), false), Ok(Vec::new()));
        assert_eq!(tracker.master(None), Vec::new());
        assert_eq!(tracker.entries()[0].state, State::Waiting);

        // Мастер появился — ставится всё, что ждёт.
        let jobs = tracker.master(Some(100));
        let first_gen = tracker.generation();
        assert_eq!(jobs, vec![(f.clone(), first_gen)]);
        assert_eq!(tracker.entries()[0].state, State::Applying);
        // При известном мастере новый проброс ставится сразу.
        assert_eq!(tracker.add(g.clone(), false), Ok(vec![(g.clone(), first_gen)]));
        assert!(tracker.finish(&f, first_gen, Ok(())));
        assert!(tracker.finish(&g, first_gen, Err("remote port forwarding failed".to_string())));
        assert_eq!(tracker.active().cloned().collect::<Vec<_>>(), vec![f.clone()]);
        assert_eq!(
            tracker.entries()[1].state,
            State::Failed("remote port forwarding failed".to_string())
        );
        // Тот же мастер — ничего не повторяется.
        assert_eq!(tracker.master(Some(100)), Vec::new());

        // Повтор неудачного проброса.
        assert_eq!(tracker.retry(&g), vec![(g.clone(), first_gen)]);
        assert_eq!(tracker.retry(&f), Vec::new(), "работающий не трогается");

        // Связь пропала: всё ждёт; ответ для старого мастера отбрасывается.
        assert_eq!(tracker.master(None), Vec::new());
        assert!(tracker.entries().iter().all(|e| e.state == State::Waiting));
        assert!(!tracker.finish(&g, first_gen, Ok(())));
        assert!(!tracker.has_master());

        // Новый вход (другой pid) — оба проброса ставятся заново.
        let jobs = tracker.master(Some(200));
        let gen2 = tracker.generation();
        assert_ne!(gen2, first_gen);
        assert_eq!(jobs, vec![(f.clone(), gen2), (g.clone(), gen2)]);
        assert!(tracker.is_applying(&f, gen2));
        // Мастер сменился, пока шла проверка: без паузы на «нет мастера».
        let jobs = tracker.master(Some(300));
        assert_eq!(jobs.len(), 2);
        assert!(!tracker.is_applying(&f, gen2));

        // Убранный во время постановки проброс ответа не получает.
        let gen3 = tracker.generation();
        assert_eq!(tracker.remove(&f), Some(true));
        assert!(!tracker.finish(&f, gen3, Ok(())));
    }
}
