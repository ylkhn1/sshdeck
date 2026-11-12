//! Сохранённые подключения: `~/.config/sshdeck/connections.ini`.
//!
//! У подключения есть название, адрес (в том же виде, что и в поле быстрого
//! подключения), необязательная команда после входа и пробросы портов,
//! которые ставятся, как только вкладка войдёт на сервер. Паролей и ключей
//! здесь нет: вход выполняет системный ssh.
//!
//! Подключения лежат деревом: в корне и в папках вперемешку подключения и
//! вложенные папки, в том порядке, который задал пользователь (новые
//! добавляются в конец). Имена папок с одним родителем не повторяются:
//! папка с тем же именем при переносе или переименовании сливается с
//! существующей. Пустых папок нет — папка появляется вместе с первым
//! подключением и исчезает с последним (подключения во вложенных папках
//! тоже считаются).
//!
//! Путь папки пишется через `/`: `Работа/Сервера`. Косая черта и обратная
//! косая внутри имени экранируются обратной косой (`\/`, `\\`) — так
//! сохраняются старые плоские папки с `/` в имени, а в обычных путях ничего
//! экранировать не нужно. Пробелы по краям частей убираются, пустые части
//! пропускаются. См. [`split_path`] и [`join_path`].
//!
//! Формат файла (ключевой файл GLib):
//!
//! - Старый (первый) формат, без группы `[sshdeck]`: группы
//!   `[connection-N]` с ключами `name`, `address`, `folder`, `startup` и
//!   `[folder-N]` с `folder` и `color`. Имя папки берётся как есть, целиком
//!   (папки одного уровня), а порядок — по алфавиту: сначала подключения вне
//!   папок, затем папки, внутри — по названию. Такой файл читается без
//!   изменений, а при следующем сохранении записывается в новом формате с
//!   этим же порядком.
//! - Новый формат начинается с `[sshdeck]` / `format=2`. Порядок — это
//!   порядок групп в файле: дерево записывается обходом в глубину, папка
//!   идёт перед своим содержимым. `folder` у папки и у подключения — полный
//!   путь папки (у подключения — той, в которой оно лежит); `[folder-N]`
//!   пишется для каждой папки, даже без цвета, чтобы запомнить её место
//!   среди соседей. Папка, которая упомянута у подключения, но не описана
//!   своей группой (файл правили руками), создаётся в этом месте.
//!   Старые версии sshdeck прочитают такой файл как плоский список с
//!   папками вида `Работа/Сервера`.
//! - Пробросы портов (необязательный ключ подключения в формате 2, номер
//!   формата не менялся): `forwards` — список GLib через `;`, каждый
//!   элемент записан как в командной строке ssh:
//!   `forwards=-L 8080:localhost:80;-D 1080;` (см. `crate::forward`).
//!   Нечитаемые элементы пропускаются. Версии без пробросов ключ не знают
//!   и при сохранении теряют его.

use std::path::{Path, PathBuf};

use gtk::glib;

use crate::forward::Forward;

const GROUP_PREFIX: &str = "connection";
const KEY_NAME: &str = "name";
const KEY_ADDRESS: &str = "address";
const KEY_FOLDER: &str = "folder";
const KEY_STARTUP: &str = "startup";
const KEY_FORWARDS: &str = "forwards";
const FOLDER_GROUP_PREFIX: &str = "folder";
const KEY_COLOR: &str = "color";
const META_GROUP: &str = "sshdeck";
const KEY_FORMAT: &str = "format";
/// Версия формата с вложенными папками и порядком пользователя.
const FORMAT_NESTED: i32 = 2;

/// Разделитель частей пути папки.
pub const PATH_SEPARATOR: char = '/';
const PATH_ESCAPE: char = '\\';

/// Цвета, которыми можно пометить папку: идентификатор в файле и название
/// в меню.
pub const FOLDER_COLORS: [(&str, &str); 7] = [
    ("red", n_!("Red")),
    ("orange", n_!("Orange")),
    ("yellow", n_!("Yellow")),
    ("green", n_!("Green")),
    ("blue", n_!("Blue")),
    ("purple", n_!("Purple")),
    ("brown", n_!("Brown")),
];

/// Путь папки: имена от корня. Пустой путь — корень (вне папок).
pub type FolderPath = Vec<String>;

/// Место элемента в дереве: номера среди соседей от корня. Пустое место —
/// сам корень.
pub type Loc = Vec<usize>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// Название в списке и на вкладке.
    pub name: String,
    /// Адрес: `user@host`, `user@host -p 2222`, псевдоним из ssh-конфига.
    pub address: String,
    /// Команда, которая отправляется в терминал сразу после входа.
    pub startup: String,
    /// Пробросы портов, которые ставятся после входа.
    pub forwards: Vec<Forward>,
}

impl Connection {
    /// Приводит поля к хранимому виду: одна строка без пробелов по краям;
    /// пустое название заменяется адресом.
    pub fn new(name: &str, address: &str) -> Self {
        let address = clean(address);
        let name = match clean(name) {
            name if name.is_empty() => address.clone(),
            name => name,
        };
        Self {
            name,
            address,
            startup: String::new(),
            forwards: Vec::new(),
        }
    }

    /// Добавляет команду после входа (одной строкой, пробелы внутри
    /// сохраняются).
    pub fn with_startup(mut self, command: &str) -> Self {
        self.startup = command
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>()
            .trim()
            .to_string();
        self
    }

    /// Задаёт пробросы портов (повторы отбрасываются).
    pub fn with_forwards(mut self, forwards: Vec<Forward>) -> Self {
        self.forwards.clear();
        for forward in forwards {
            if !self.forwards.contains(&forward) {
                self.forwards.push(forward);
            }
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub name: String,
    /// Идентификатор из [`FOLDER_COLORS`].
    pub color: Option<String>,
    pub children: Vec<Node>,
}

impl Folder {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            color: None,
            children: Vec::new(),
        }
    }

    /// Сколько подключений в папке вместе с вложенными.
    pub fn count(&self) -> usize {
        count(&self.children)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Connection(Connection),
    Folder(Folder),
}

/// Элемент дерева при обходе в глубину.
#[derive(Debug)]
pub struct Item<'a> {
    pub loc: Loc,
    /// Путь папки, в которой лежит элемент.
    pub parent: FolderPath,
    pub node: &'a Node,
}

impl Item<'_> {
    /// Уровень вложенности: 0 — корень.
    pub fn depth(&self) -> usize {
        self.parent.len()
    }
}

/// Куда перенести элемент.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Перед элементом, в ту же папку.
    Before(Loc),
    /// После элемента, в ту же папку.
    After(Loc),
    /// В конец папки (пустое место — в конец корня).
    Into(Loc),
}

/// Одна строка: управляющие символы и повторные пробелы схлопываются.
pub fn clean(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Разбирает путь папки, набранный пользователем или прочитанный из файла.
/// `\/` — косая черта внутри имени, `\\` — обратная косая; другие
/// обратные косые остаются как есть.
pub fn split_path(text: &str) -> FolderPath {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            PATH_ESCAPE => match chars.peek() {
                Some(&next) if next == PATH_SEPARATOR || next == PATH_ESCAPE => {
                    current.push(next);
                    chars.next();
                }
                _ => current.push(c),
            },
            PATH_SEPARATOR => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    parts.push(current);
    parts
        .iter()
        .map(|part| clean(part))
        .filter(|part| !part.is_empty())
        .collect()
}

/// Записывает путь так, чтобы [`split_path`] вернул его без изменений.
pub fn join_path(path: &[String]) -> String {
    path.iter()
        .map(|part| {
            let mut escaped = String::with_capacity(part.len());
            for c in part.chars() {
                if c == PATH_SEPARATOR || c == PATH_ESCAPE {
                    escaped.push(PATH_ESCAPE);
                }
                escaped.push(c);
            }
            escaped
        })
        .collect::<Vec<_>>()
        .join(&PATH_SEPARATOR.to_string())
}

fn count(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|node| match node {
            Node::Connection(_) => 1,
            Node::Folder(folder) => folder.count(),
        })
        .sum()
}

/// Подключение или непустая папка: такой элемент переживёт чистку.
fn alive(node: &Node) -> bool {
    match node {
        Node::Connection(_) => true,
        Node::Folder(folder) => folder.count() > 0,
    }
}

/// Убирает пустые папки на всех уровнях.
fn prune(nodes: &mut Vec<Node>) {
    nodes.retain(alive);
    for node in nodes {
        if let Node::Folder(folder) = node {
            prune(&mut folder.children);
        }
    }
}

/// Вставляет элемент на место `index`. Папка с именем, которое уже есть
/// среди соседей, сливается с той: её содержимое уходит в конец
/// существующей, цвет остаётся прежним (или берётся у вставляемой, если
/// своего не было). Возвращает номер, под которым элемент оказался.
fn insert_merge(siblings: &mut Vec<Node>, index: usize, node: Node) -> usize {
    if let Node::Folder(folder) = node {
        let existing = siblings
            .iter()
            .position(|n| matches!(n, Node::Folder(f) if f.name == folder.name));
        match existing {
            Some(position) => {
                let Node::Folder(target) = &mut siblings[position] else {
                    unreachable!();
                };
                if target.color.is_none() {
                    target.color = folder.color;
                }
                for child in folder.children {
                    let end = target.children.len();
                    insert_merge(&mut target.children, end, child);
                }
                position
            }
            None => {
                let index = index.min(siblings.len());
                siblings.insert(index, Node::Folder(folder));
                index
            }
        }
    } else {
        let index = index.min(siblings.len());
        siblings.insert(index, node);
        index
    }
}

/// Поправляет место `loc` после того, как элемент `removed` вынули из
/// дерева: соседи после него сдвигаются на одну позицию.
fn shift_after_removal(loc: &mut Loc, removed: &[usize]) {
    let Some((&index, parent)) = removed.split_last() else {
        return;
    };
    let level = parent.len();
    if loc.len() > level && loc[..level] == *parent && loc[level] > index {
        loc[level] -= 1;
    }
}

fn walk_into<'a>(nodes: &'a [Node], loc: &mut Loc, parent: &mut FolderPath, out: &mut Vec<Item<'a>>) {
    for (index, node) in nodes.iter().enumerate() {
        loc.push(index);
        out.push(Item {
            loc: loc.clone(),
            parent: parent.clone(),
            node,
        });
        if let Node::Folder(folder) = node {
            parent.push(folder.name.clone());
            walk_into(&folder.children, loc, parent, out);
            parent.pop();
        }
        loc.pop();
    }
}

/// Порядок старого формата: подключения по названию, за ними папки по
/// имени (без учёта регистра, при равенстве — с учётом).
fn sort_alphabetically(nodes: &mut [Node]) {
    nodes.sort_by_cached_key(|node| match node {
        Node::Connection(c) => (false, c.name.to_lowercase(), c.name.clone()),
        Node::Folder(f) => (true, f.name.to_lowercase(), f.name.clone()),
    });
    for node in nodes {
        if let Node::Folder(folder) = node {
            sort_alphabetically(&mut folder.children);
        }
    }
}

fn valid_color(color: &str) -> bool {
    FOLDER_COLORS.iter().any(|(id, _)| *id == color)
}

/// Дерево подключений и папок в порядке пользователя.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connections {
    root: Vec<Node>,
}

pub fn default_path() -> PathBuf {
    glib::user_config_dir()
        .join("sshdeck")
        .join("connections.ini")
}

impl Connections {
    /// Читает файл. Отсутствующий файл — это пустой список, а не ошибка.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(data) => Self::parse(&data).map_err(|err| format!("{}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(format!("{}: {err}", path.display())),
        }
    }

    pub fn parse(data: &str) -> Result<Self, String> {
        let key_file = glib::KeyFile::new();
        key_file
            .load_from_data(data, glib::KeyFileFlags::NONE)
            .map_err(|err| err.to_string())?;
        let nested = key_file.integer(META_GROUP, KEY_FORMAT).is_ok_and(|v| v >= FORMAT_NESTED);
        // В старом формате имя папки — одна часть пути, без экранирования.
        let path_of = |text: &str| -> FolderPath {
            if nested {
                split_path(text)
            } else {
                let name = clean(text);
                if name.is_empty() { Vec::new() } else { vec![name] }
            }
        };
        let mut this = Self::default();
        // Цвета старого формата: группы папок идут после подключений, а
        // папку без подключений создавать не нужно.
        let mut old_colors = Vec::new();
        for group in key_file.groups().iter() {
            let group = group.as_str();
            let field = |key: &str| {
                key_file
                    .string(group, key)
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            };
            if group == META_GROUP {
                continue;
            }
            if group.starts_with(FOLDER_GROUP_PREFIX) {
                let path = path_of(&field(KEY_FOLDER));
                if path.is_empty() {
                    continue;
                }
                let color = Some(field(KEY_COLOR)).filter(|color| valid_color(color));
                if nested {
                    let loc = this.ensure_folder(&path);
                    if let Some(Node::Folder(folder)) = this.node_mut(&loc)
                        && color.is_some()
                    {
                        folder.color = color;
                    }
                } else if let Some(color) = color {
                    old_colors.push((path, color));
                }
                continue;
            }
            if !group.starts_with(GROUP_PREFIX) {
                continue;
            }
            let forwards = key_file
                .string_list(group, KEY_FORWARDS)
                .map(|list| {
                    list.iter()
                        .filter_map(|item| Forward::parse(item.as_str()).ok())
                        .collect()
                })
                .unwrap_or_default();
            let connection = Connection::new(&field(KEY_NAME), &field(KEY_ADDRESS))
                .with_startup(&field(KEY_STARTUP))
                .with_forwards(forwards);
            // Запись без адреса бесполезна: подключаться некуда.
            if !connection.address.is_empty() {
                this.add(connection, &path_of(&field(KEY_FOLDER)));
            }
        }
        if !nested {
            sort_alphabetically(&mut this.root);
            for (path, color) in old_colors {
                if let Some(loc) = this.folder_loc(&path) {
                    this.set_folder_color(&loc, Some(&color));
                }
            }
        }
        prune(&mut this.root);
        Ok(this)
    }

    pub fn serialize(&self) -> String {
        let key_file = glib::KeyFile::new();
        key_file.set_integer(META_GROUP, KEY_FORMAT, FORMAT_NESTED);
        let (mut connections, mut folders) = (0, 0);
        for item in self.walk() {
            match item.node {
                Node::Folder(folder) => {
                    folders += 1;
                    let group = format!("{FOLDER_GROUP_PREFIX}-{folders}");
                    let mut path = item.parent.clone();
                    path.push(folder.name.clone());
                    key_file.set_string(&group, KEY_FOLDER, &join_path(&path));
                    if let Some(color) = &folder.color {
                        key_file.set_string(&group, KEY_COLOR, color);
                    }
                }
                Node::Connection(c) => {
                    connections += 1;
                    let group = format!("{GROUP_PREFIX}-{connections}");
                    key_file.set_string(&group, KEY_NAME, &c.name);
                    key_file.set_string(&group, KEY_ADDRESS, &c.address);
                    if !item.parent.is_empty() {
                        key_file.set_string(&group, KEY_FOLDER, &join_path(&item.parent));
                    }
                    if !c.startup.is_empty() {
                        key_file.set_string(&group, KEY_STARTUP, &c.startup);
                    }
                    if !c.forwards.is_empty() {
                        // Список GLib: элементы с `;` в конце. В записи
                        // проброса нет ни `;`, ни `\`, экранировать нечего.
                        let list: String = c.forwards.iter().map(|f| format!("{f};")).collect();
                        key_file.set_string(&group, KEY_FORWARDS, &list);
                    }
                }
            }
        }
        key_file.to_data().to_string()
    }

    /// Записывает файл через временный, чтобы сбой не оставил половину
    /// списка.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let describe = |target: &Path, err: std::io::Error| format!("{}: {err}", target.display());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| describe(dir, err))?;
        }
        let mut temp = path.as_os_str().to_owned();
        temp.push(".tmp");
        let temp = PathBuf::from(temp);
        std::fs::write(&temp, self.serialize()).map_err(|err| describe(&temp, err))?;
        std::fs::rename(&temp, path).map_err(|err| describe(path, err))
    }

    // ----- чтение

    pub fn is_empty(&self) -> bool {
        self.root.is_empty()
    }

    /// Все элементы обходом в глубину: папка перед своим содержимым.
    pub fn walk(&self) -> Vec<Item<'_>> {
        let mut out = Vec::new();
        walk_into(&self.root, &mut Vec::new(), &mut Vec::new(), &mut out);
        out
    }

    pub fn node(&self, loc: &[usize]) -> Option<&Node> {
        let (&last, parent) = loc.split_last()?;
        self.children(parent)?.get(last)
    }

    fn node_mut(&mut self, loc: &[usize]) -> Option<&mut Node> {
        let (&last, parent) = loc.split_last()?;
        self.children_mut(parent)?.get_mut(last)
    }

    /// Содержимое папки (или корня для пустого места).
    fn children(&self, loc: &[usize]) -> Option<&Vec<Node>> {
        let mut nodes = &self.root;
        for &index in loc {
            match nodes.get(index)? {
                Node::Folder(folder) => nodes = &folder.children,
                Node::Connection(_) => return None,
            }
        }
        Some(nodes)
    }

    fn children_mut(&mut self, loc: &[usize]) -> Option<&mut Vec<Node>> {
        let mut nodes = &mut self.root;
        for &index in loc {
            match nodes.get_mut(index)? {
                Node::Folder(folder) => nodes = &mut folder.children,
                Node::Connection(_) => return None,
            }
        }
        Some(nodes)
    }

    pub fn connection(&self, loc: &[usize]) -> Option<&Connection> {
        match self.node(loc)? {
            Node::Connection(c) => Some(c),
            Node::Folder(_) => None,
        }
    }

    pub fn folder(&self, loc: &[usize]) -> Option<&Folder> {
        match self.node(loc)? {
            Node::Folder(folder) => Some(folder),
            Node::Connection(_) => None,
        }
    }

    /// Первое подключение с такими же полями.
    pub fn find_connection(&self, connection: &Connection) -> Option<Loc> {
        self.walk()
            .into_iter()
            .find(|item| matches!(item.node, Node::Connection(c) if c == connection))
            .map(|item| item.loc)
    }

    /// Место папки по пути; для пустого пути — корень.
    pub fn folder_loc(&self, path: &[String]) -> Option<Loc> {
        let mut loc = Vec::new();
        let mut nodes = &self.root;
        for name in path {
            let index = nodes
                .iter()
                .position(|n| matches!(n, Node::Folder(f) if &f.name == name))?;
            let Node::Folder(folder) = &nodes[index] else {
                unreachable!();
            };
            loc.push(index);
            nodes = &folder.children;
        }
        Some(loc)
    }

    /// Путь папки, в которой лежит элемент `loc`.
    pub fn parent_path(&self, loc: &[usize]) -> FolderPath {
        let mut path = Vec::new();
        let mut nodes = &self.root;
        for &index in loc.iter().take(loc.len().saturating_sub(1)) {
            match nodes.get(index) {
                Some(Node::Folder(folder)) => {
                    path.push(folder.name.clone());
                    nodes = &folder.children;
                }
                _ => break,
            }
        }
        path
    }

    /// Полный путь папки `loc` (вместе с её именем).
    pub fn folder_path(&self, loc: &[usize]) -> Option<FolderPath> {
        let folder = self.folder(loc)?;
        let mut path = self.parent_path(loc);
        path.push(folder.name.clone());
        Some(path)
    }

    /// Пути всех папок обходом в глубину.
    pub fn folders(&self) -> Vec<FolderPath> {
        self.walk()
            .into_iter()
            .filter_map(|item| match item.node {
                Node::Folder(folder) => {
                    let mut path = item.parent;
                    path.push(folder.name.clone());
                    Some(path)
                }
                Node::Connection(_) => None,
            })
            .collect()
    }

    /// Цвет папки, если он задан.
    pub fn folder_color(&self, loc: &[usize]) -> Option<&str> {
        self.folder(loc)?.color.as_deref()
    }

    /// Можно ли сдвинуть элемент на `delta` позиций среди соседей.
    pub fn can_move_by(&self, loc: &[usize], delta: isize) -> bool {
        let Some((&index, parent)) = loc.split_last() else {
            return false;
        };
        let Some(siblings) = self.children(parent) else {
            return false;
        };
        index < siblings.len()
            && index
                .checked_add_signed(delta)
                .is_some_and(|target| target < siblings.len())
    }

    // ----- изменение

    /// Находит папку по пути, создавая недостающие в конце родителя.
    fn ensure_folder(&mut self, path: &[String]) -> Loc {
        let mut loc = Vec::new();
        let mut nodes = &mut self.root;
        for name in path {
            let index = match nodes
                .iter()
                .position(|n| matches!(n, Node::Folder(f) if &f.name == name))
            {
                Some(index) => index,
                None => {
                    nodes.push(Node::Folder(Folder::new(name)));
                    nodes.len() - 1
                }
            };
            loc.push(index);
            let Node::Folder(folder) = &mut nodes[index] else {
                unreachable!();
            };
            nodes = &mut folder.children;
        }
        loc
    }

    /// Добавляет подключение в конец папки (недостающие папки создаются).
    /// Возвращает его место.
    pub fn add(&mut self, connection: Connection, folder: &[String]) -> Loc {
        let mut loc = self.ensure_folder(folder);
        // Папка только что найдена или создана.
        if let Some(nodes) = self.children_mut(&loc) {
            nodes.push(Node::Connection(connection));
            loc.push(nodes.len() - 1);
        }
        loc
    }

    /// Заменяет подключение. В той же папке оно остаётся на своём месте,
    /// в другую переезжает в конец. Возвращает новое место; `None`, если
    /// по `loc` не подключение.
    pub fn replace(&mut self, loc: &[usize], connection: Connection, folder: &[String]) -> Option<Loc> {
        self.connection(loc)?;
        if self.parent_path(loc) == folder {
            if let Some(Node::Connection(slot)) = self.node_mut(loc) {
                *slot = connection;
            }
            return Some(loc.to_vec());
        }
        self.take(loc);
        let new = self.add(connection, folder);
        Some(self.settle(&new))
    }

    /// Убирает элемент (папку — вместе с содержимым).
    pub fn remove(&mut self, loc: &[usize]) -> Option<Node> {
        let node = self.take(loc)?;
        prune(&mut self.root);
        Some(node)
    }

    fn take(&mut self, loc: &[usize]) -> Option<Node> {
        let (&index, parent) = loc.split_last()?;
        let siblings = self.children_mut(parent)?;
        (index < siblings.len()).then(|| siblings.remove(index))
    }

    /// Чистит пустые папки и возвращает, где после этого окажется элемент
    /// `loc` (сам он и его родители не пустые и остаются).
    fn settle(&mut self, loc: &[usize]) -> Loc {
        let mut settled = Vec::with_capacity(loc.len());
        for level in 0..loc.len() {
            let alive_before = self.children(&loc[..level]).map_or(0, |siblings| {
                siblings[..loc[level].min(siblings.len())]
                    .iter()
                    .filter(|n| alive(n))
                    .count()
            });
            settled.push(alive_before);
        }
        prune(&mut self.root);
        settled
    }

    /// Меняет элемент местами с соседом (`delta` = −1 — вверх, 1 — вниз).
    pub fn move_by(&mut self, loc: &[usize], delta: isize) -> Option<Loc> {
        if !self.can_move_by(loc, delta) {
            return None;
        }
        let (&index, parent) = loc.split_last()?;
        let target = index.checked_add_signed(delta)?;
        self.children_mut(parent)?.swap(index, target);
        let mut new = parent.to_vec();
        new.push(target);
        Some(new)
    }

    /// Переносит элемент. Папку нельзя перенести в неё саму или во
    /// вложенную в неё. Возвращает новое место элемента (для папки, слитой
    /// с одноимённой, — место той).
    pub fn move_to(&mut self, loc: &[usize], target: &Target) -> Option<Loc> {
        let anchor = match target {
            Target::Before(anchor) | Target::After(anchor) => {
                self.node(anchor)?;
                anchor
            }
            Target::Into(anchor) => {
                self.children(anchor)?;
                anchor
            }
        };
        self.node(loc)?;
        if anchor.starts_with(loc) {
            return None;
        }
        let node = self.take(loc)?;
        let mut anchor = anchor.clone();
        shift_after_removal(&mut anchor, loc);
        let (parent, index) = match target {
            Target::Before(_) => {
                let (&index, parent) = anchor.split_last()?;
                (parent.to_vec(), index)
            }
            Target::After(_) => {
                let (&index, parent) = anchor.split_last()?;
                (parent.to_vec(), index + 1)
            }
            Target::Into(_) => {
                let len = self.children(&anchor)?.len();
                (anchor, len)
            }
        };
        let siblings = self.children_mut(&parent)?;
        let index = insert_merge(siblings, index, node);
        let mut new = parent;
        new.push(index);
        Some(self.settle(&new))
    }

    /// Переносит элемент в конец папки по пути, создавая её при
    /// необходимости. Если элемент уже лежит в этой папке, ничего не
    /// меняется.
    pub fn move_into(&mut self, loc: &[usize], folder: &[String]) -> Option<Loc> {
        let node = self.node(loc)?;
        if self.parent_path(loc) == folder {
            return Some(loc.to_vec());
        }
        if let Node::Folder(_) = node
            && folder.starts_with(&self.folder_path(loc)?)
        {
            return None;
        }
        // Новые папки создаются в конце родителя и не сдвигают `loc`.
        let anchor = self.ensure_folder(folder);
        self.move_to(loc, &Target::Into(anchor))
    }

    /// Переименовывает папку; содержимое остаётся в ней. Если у соседей
    /// уже есть папка с таким именем, папки сливаются. Возвращает место
    /// папки.
    pub fn rename_folder(&mut self, loc: &[usize], name: &str) -> Option<Loc> {
        let name = clean(name);
        if name.is_empty() {
            return None;
        }
        let (&index, parent) = loc.split_last()?;
        let siblings = self.children_mut(parent)?;
        let Some(Node::Folder(folder)) = siblings.get_mut(index) else {
            return None;
        };
        if folder.name == name {
            return Some(loc.to_vec());
        }
        folder.name = name.clone();
        let other = siblings
            .iter()
            .enumerate()
            .position(|(i, n)| i != index && matches!(n, Node::Folder(f) if f.name == name));
        let Some(other) = other else {
            return Some(loc.to_vec());
        };
        let node = siblings.remove(index);
        let other = if other > index { other - 1 } else { other };
        // Слияние с существующей: она остаётся на своём месте.
        let merged = insert_merge(siblings, other, node);
        let mut new = parent.to_vec();
        new.push(merged);
        Some(new)
    }

    /// Убирает папку, оставляя содержимое: подключения и вложенные папки
    /// поднимаются на уровень выше, на место папки.
    pub fn dissolve_folder(&mut self, loc: &[usize]) -> bool {
        if self.folder(loc).is_none() {
            return false;
        }
        let Some(Node::Folder(folder)) = self.take(loc) else {
            return false;
        };
        let Some((&index, parent)) = loc.split_last() else {
            return false;
        };
        let Some(siblings) = self.children_mut(parent) else {
            return false;
        };
        let mut position = index;
        for child in folder.children {
            let len = siblings.len();
            insert_merge(siblings, position, child);
            // Если элемент слился с одноимённой папкой, позиция не растёт.
            if siblings.len() > len {
                position += 1;
            }
        }
        prune(&mut self.root);
        true
    }

    /// Удаляет папку вместе с содержимым. Возвращает число удалённых
    /// подключений.
    pub fn delete_folder(&mut self, loc: &[usize]) -> usize {
        if self.folder(loc).is_none() {
            return 0;
        }
        match self.remove(loc) {
            Some(Node::Folder(folder)) => folder.count(),
            _ => 0,
        }
    }

    /// Задаёт цвет папки; `None` или неизвестный идентификатор убирают его.
    pub fn set_folder_color(&mut self, loc: &[usize], color: Option<&str>) {
        if let Some(Node::Folder(folder)) = self.node_mut(loc) {
            folder.color = color.filter(|color| valid_color(color)).map(str::to_string);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Connection, Connections, FolderPath, Node, Target, clean, join_path, split_path};

    fn path(text: &str) -> FolderPath {
        split_path(text)
    }

    /// Дерево одной строкой: `имя` — подключение, `[папка: …]` — папка.
    fn outline(list: &Connections) -> String {
        fn render(nodes: &[Node]) -> String {
            nodes
                .iter()
                .map(|node| match node {
                    Node::Connection(c) => c.name.clone(),
                    Node::Folder(f) => format!("[{}: {}]", f.name, render(&f.children)),
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
        render(&list.root)
    }

    fn c(name: &str) -> Connection {
        Connection::new(name, &format!("root@{name}"))
    }

    /// Корень: «Домашний», [Прод: web, db, [EU: fra1, [Резерв: ams]]], [Тест: стенд].
    fn sample() -> Connections {
        let mut list = Connections::default();
        list.add(c("Домашний"), &[]);
        list.add(c("web"), &path("Прод"));
        list.add(c("db"), &path("Прод"));
        list.add(c("fra1"), &path("Прод/EU"));
        list.add(c("ams"), &path("Прод/EU/Резерв"));
        list.add(c("стенд"), &path("Тест"));
        list
    }

    fn loc_of(list: &Connections, name: &str) -> Vec<usize> {
        list.walk()
            .into_iter()
            .find(|item| match item.node {
                Node::Connection(c) => c.name == name,
                Node::Folder(f) => f.name == name,
            })
            .map(|item| item.loc)
            .unwrap_or_else(|| panic!("нет {name}"))
    }

    #[test]
    fn fields_are_cleaned() {
        let c = Connection::new("  Мой\tсервер \n", " u@h  -p 22 ");
        assert_eq!(c.name, "Мой сервер");
        assert_eq!(c.address, "u@h -p 22");
        assert_eq!(clean("   "), "");
    }

    #[test]
    fn startup_command_is_kept_and_saved() {
        let c = Connection::new("x", "u@h").with_startup("  cd /srv  &&  ls\n");
        assert_eq!(c.startup, "cd /srv  &&  ls");
        let mut list = Connections::default();
        list.add(c.clone(), &[]);
        let restored = Connections::parse(&list.serialize()).expect("parse");
        assert_eq!(restored.connection(&[0]), Some(&c));
    }

    #[test]
    fn forwards_are_saved_and_loaded() {
        let forwards = crate::forward::parse_list("-L 8080:localhost:80, -R [::1]:9000:localhost:3000, -D *:1080")
            .expect("forwards");
        let c = Connection::new("web", "u@h").with_startup("htop").with_forwards(forwards.clone());
        let mut list = Connections::default();
        list.add(c.clone(), &path("Прод"));
        list.add(Connection::new("без пробросов", "u@x"), &[]);
        let saved = list.serialize();
        assert!(
            saved.contains("forwards=-L 8080:localhost:80;-R [::1]:9000:localhost:3000;-D *:1080;"),
            "{saved}"
        );
        assert_eq!(saved.matches("forwards=").count(), 1, "{saved}");
        let restored = Connections::parse(&saved).expect("parse");
        assert_eq!(restored, list);
        assert_eq!(restored.connection(&[0, 0]).expect("web").forwards, forwards);
    }

    #[test]
    fn files_without_or_with_broken_forwards_still_load() {
        // Файл старой версии (формат 2 без пробросов) и запись, поправленная
        // руками: нечитаемый проброс и повтор пропускаются.
        let data = "[sshdeck]\nformat=2\n\n\
[connection-1]\nname=a\naddress=u@a\n\n\
[connection-2]\nname=b\naddress=u@b\nforwards=-D 1080;мусор;-L 99999:h:1;-D 1080;-L 5432:db:5432\n";
        let list = Connections::parse(data).expect("parse");
        assert_eq!(list.connection(&[0]).expect("a").forwards, Vec::new());
        assert_eq!(
            crate::forward::format_list(&list.connection(&[1]).expect("b").forwards),
            "-D 1080, -L 5432:db:5432"
        );
        // Старый плоский формат тоже читается.
        let old = "[connection-1]\nname=x\naddress=u@x\nforwards=-D 1081\n";
        let list = Connections::parse(old).expect("parse");
        assert_eq!(list.connection(&[0]).expect("x").forwards.len(), 1);
    }

    #[test]
    fn empty_name_falls_back_to_address() {
        assert_eq!(Connection::new(" ", "u@h").name, "u@h");
    }

    #[test]
    fn paths_are_split_and_joined() {
        assert_eq!(path("Работа/Сервера"), ["Работа", "Сервера"]);
        assert_eq!(path(" / a //  b  c / "), ["a", "b c"]);
        assert!(path("").is_empty());
        assert_eq!(path(r"Прод\/EU/x"), ["Прод/EU", "x"]);
        assert_eq!(path(r"a\\/b"), [r"a\", "b"]);
        // Обратная косая перед обычной буквой остаётся.
        assert_eq!(path(r"C:\temp"), [r"C:\temp"]);
        for parts in [
            vec!["Прод/EU".to_string(), r"a\b".to_string()],
            vec![r"конец\".to_string()],
            vec!["просто".to_string(), "путь".to_string()],
        ] {
            assert_eq!(split_path(&join_path(&parts)), parts, "{parts:?}");
        }
        assert_eq!(join_path(&path("Работа/Сервера")), "Работа/Сервера");
    }

    #[test]
    fn new_items_keep_insertion_order() {
        let list = sample();
        assert_eq!(
            outline(&list),
            "Домашний, [Прод: web, db, [EU: fra1, [Резерв: ams]]], [Тест: стенд]"
        );
        assert_eq!(super::count(&list.root), 6);
        assert_eq!(
            list.folders(),
            [path("Прод"), path("Прод/EU"), path("Прод/EU/Резерв"), path("Тест")]
        );
        let prod = list.folder_loc(&path("Прод")).expect("Прод");
        assert_eq!(list.folder(&prod).map(|f| f.count()), Some(4));
        assert_eq!(list.parent_path(&loc_of(&list, "ams")), path("Прод/EU/Резерв"));
        assert_eq!(list.folder_path(&loc_of(&list, "EU")), Some(path("Прод/EU")));
    }

    #[test]
    fn old_flat_file_loads_alphabetically() {
        // Файл версии без вложенных папок: порядок групп случайный, у
        // папки «Прод/EU» в имени косая черта.
        let data = "\
[connection-1]\nname=web\naddress=root@10.0.0.2\nfolder=Прод\n\n\
[connection-2]\nname=Домашний\naddress=me@home -p 2222\n\n\
[connection-3]\nname=db\naddress=root@10.0.0.3\nfolder=Прод\n\n\
[connection-4]\nname=стенд\naddress=dev@stand\nfolder=Тест\nstartup=cd /srv\n\n\
[connection-5]\nname=Альфа\naddress=a@b\nfolder=Прод/EU\n\n\
[connection-6]\nname=ещё\naddress=e@f\n\n\
[folder-1]\nfolder=Прод\ncolor=red\n\n\
[folder-2]\nfolder=Призрак\ncolor=green\n\n\
[folder-3]\nfolder=Тест\ncolor=no-such-color\n";
        let list = Connections::parse(data).expect("parse");
        assert_eq!(
            outline(&list),
            "Домашний, ещё, [Прод: db, web], [Прод/EU: Альфа], [Тест: стенд]"
        );
        let prod = list.folder_loc(&path("Прод")).expect("Прод");
        assert_eq!(list.folder_color(&prod), Some("red"));
        let test = list.folder_loc(&path("Тест")).expect("Тест");
        assert_eq!(list.folder_color(&test), None);
        assert!(list.folder_loc(&path("Призрак")).is_none());
        // Папка со старым именем «Прод/EU» — одна, верхнего уровня.
        assert_eq!(list.folder_loc(&[String::from("Прод/EU")]), Some(vec![3]));

        // Пересохранение в новом формате ничего не теряет и не меняет порядок.
        let saved = list.serialize();
        assert!(saved.contains("format=2"));
        assert!(saved.contains(r"folder=Прод\\/EU"), "{saved}");
        assert_eq!(Connections::parse(&saved), Ok(list));
    }

    #[test]
    fn new_format_round_trip_keeps_order_nesting_and_colors() {
        let mut list = sample();
        // Порядок не алфавитный и вперемешку с папками.
        list.add(c("последний в корне"), &[]);
        list.add(Connection::new("хитрое; имя = [x] # y", "u@h").with_startup("ls -la"), &path(r"a;b/c\/d"));
        let eu = list.folder_loc(&path("Прод/EU")).expect("EU");
        list.set_folder_color(&eu, Some("blue"));
        let saved = list.serialize();
        let restored = Connections::parse(&saved).expect("parse");
        assert_eq!(restored, list);
        assert_eq!(
            outline(&restored),
            "Домашний, [Прод: web, db, [EU: fra1, [Резерв: ams]]], [Тест: стенд], \
             последний в корне, [a;b: [c/d: хитрое; имя = [x] # y]]"
        );
        assert_eq!(restored.folder_color(&eu), Some("blue"));
    }

    #[test]
    fn hand_edited_new_file_creates_missing_folders() {
        let data = "[sshdeck]\nformat=2\n\n\
[connection-1]\nname=b\naddress=u@b\nfolder=X/Y\n\n\
[folder-1]\nfolder=Пустая\ncolor=red\n\n\
[connection-2]\nname=a\naddress=u@a\n\n\
[folder-2]\nfolder=X\ncolor=green\n";
        let list = Connections::parse(data).expect("parse");
        assert_eq!(outline(&list), "[X: [Y: b]], a");
        assert_eq!(list.folder_color(&[0]), Some("green"));
    }

    #[test]
    fn records_without_address_are_dropped() {
        let data = "[connection-1]\nname=x\n\n[connection-2]\nname=y\naddress=u@h\n\n[other]\na=b\n";
        let list = Connections::parse(data).expect("parse");
        assert_eq!(outline(&list), "y");
    }

    #[test]
    fn broken_file_is_an_error() {
        assert!(Connections::parse("это не ini").is_err());
    }

    #[test]
    fn replace_keeps_place_or_moves_to_the_end_of_another_folder() {
        let mut list = sample();
        let web = loc_of(&list, "web");
        assert_eq!(list.replace(&web, c("веб"), &path("Прод")), Some(web.clone()));
        assert_eq!(
            outline(&list),
            "Домашний, [Прод: веб, db, [EU: fra1, [Резерв: ams]]], [Тест: стенд]"
        );
        // Последнее подключение уходит из «Тест»: папка исчезает.
        let stand = loc_of(&list, "стенд");
        let new = list.replace(&stand, c("стенд"), &path("Прод/EU")).expect("replace");
        assert_eq!(
            outline(&list),
            "Домашний, [Прод: веб, db, [EU: fra1, [Резерв: ams], стенд]]"
        );
        assert_eq!(list.connection(&new).map(|c| c.name.as_str()), Some("стенд"));
        assert!(list.replace(&[99], c("x"), &[]).is_none());
        assert!(list.replace(&[1], c("x"), &[]).is_none(), "это папка");
    }

    #[test]
    fn remove_prunes_empty_folders() {
        let mut list = sample();
        let ams = loc_of(&list, "ams");
        assert!(matches!(list.remove(&ams), Some(Node::Connection(_))));
        assert_eq!(
            outline(&list),
            "Домашний, [Прод: web, db, [EU: fra1]], [Тест: стенд]"
        );
        assert!(list.remove(&[99]).is_none());
        let found = list.find_connection(&c("db"));
        assert_eq!(found, Some(vec![1, 1]));
    }

    #[test]
    fn move_up_and_down_among_siblings() {
        let mut list = sample();
        let db = loc_of(&list, "db");
        assert!(list.can_move_by(&db, -1));
        assert_eq!(list.move_by(&db, -1), Some(vec![1, 0]));
        assert_eq!(list.move_by(&[1, 0], -1), None, "уже первое");
        assert!(!list.can_move_by(&[2], 1), "последнее в корне");
        // Папка двигается вместе с содержимым.
        assert_eq!(list.move_by(&[1], 1), Some(vec![2]));
        assert_eq!(
            outline(&list),
            "Домашний, [Тест: стенд], [Прод: db, web, [EU: fra1, [Резерв: ams]]]"
        );
    }

    #[test]
    fn drag_targets_before_after_and_into() {
        let mut list = sample();
        // Подключение из корня — перед вложенным в папку.
        let home = loc_of(&list, "Домашний");
        let fra1 = loc_of(&list, "fra1");
        let new = list.move_to(&home, &Target::Before(fra1)).expect("before");
        assert_eq!(
            outline(&list),
            "[Прод: web, db, [EU: Домашний, fra1, [Резерв: ams]]], [Тест: стенд]"
        );
        assert_eq!(new, loc_of(&list, "Домашний"));

        // Среди соседей той же папки вниз.
        let web = loc_of(&list, "web");
        let db = loc_of(&list, "db");
        assert_eq!(list.move_to(&web, &Target::After(db)), Some(vec![0, 1]));
        assert_eq!(
            outline(&list),
            "[Прод: db, web, [EU: Домашний, fra1, [Резерв: ams]]], [Тест: стенд]"
        );

        // Последнее подключение «Тест» уходит в корень: папка исчезает,
        // место пересчитано с учётом этого.
        let stand = loc_of(&list, "стенд");
        let prod = loc_of(&list, "Прод");
        assert_eq!(list.move_to(&stand, &Target::Before(prod)), Some(vec![0]));
        assert_eq!(
            outline(&list),
            "стенд, [Прод: db, web, [EU: Домашний, fra1, [Резерв: ams]]]"
        );

        // Папка — в конец корня, с поддеревом.
        let reserve = loc_of(&list, "Резерв");
        assert_eq!(list.move_to(&reserve, &Target::Into(vec![])), Some(vec![2]));
        assert_eq!(
            outline(&list),
            "стенд, [Прод: db, web, [EU: Домашний, fra1]], [Резерв: ams]"
        );

        // Подключение — в конец папки.
        let stand = loc_of(&list, "стенд");
        let reserve = loc_of(&list, "Резерв");
        assert!(list.move_to(&stand, &Target::Into(reserve)).is_some());
        assert_eq!(
            outline(&list),
            "[Прод: db, web, [EU: Домашний, fra1]], [Резерв: ams, стенд]"
        );
    }

    #[test]
    fn folder_cannot_move_into_itself() {
        let mut list = sample();
        let before = list.clone();
        let prod = loc_of(&list, "Прод");
        let eu = loc_of(&list, "EU");
        let fra1 = loc_of(&list, "fra1");
        assert_eq!(list.move_to(&prod, &Target::Into(eu.clone())), None);
        assert_eq!(list.move_to(&prod, &Target::Into(prod.clone())), None);
        assert_eq!(list.move_to(&prod, &Target::Before(fra1)), None);
        assert_eq!(list.move_to(&prod, &Target::After(prod.clone())), None);
        assert_eq!(list.move_into(&prod, &path("Прод/EU/Новая")), None);
        assert_eq!(list.move_to(&[99], &Target::Into(vec![])), None);
        assert_eq!(list.move_to(&eu, &Target::Into(vec![0])), None, "в подключение нельзя");
        assert_eq!(list, before);
    }

    #[test]
    fn moved_folder_merges_with_namesake() {
        let mut list = sample();
        list.add(c("x"), &path("EU"));
        let eu_root = loc_of(&list, "x")[..1].to_vec();
        let red = list.folder_loc(&path("EU")).expect("EU");
        list.set_folder_color(&red, Some("red"));
        // [EU: x] из корня — в «Прод», где уже есть EU.
        let new = list.move_to(&eu_root, &Target::Into(vec![1])).expect("move");
        assert_eq!(
            outline(&list),
            "Домашний, [Прод: web, db, [EU: fra1, [Резерв: ams], x]], [Тест: стенд]"
        );
        assert_eq!(list.folder_path(&new), Some(path("Прод/EU")));
        assert_eq!(list.folder_color(&new), Some("red"), "цвет взят у слитой");
    }

    #[test]
    fn move_into_path_creates_folders() {
        let mut list = sample();
        let home = loc_of(&list, "Домашний");
        let new = list.move_into(&home, &path("Дом/Роутеры")).expect("move");
        assert_eq!(
            outline(&list),
            "[Прод: web, db, [EU: fra1, [Резерв: ams]]], [Тест: стенд], [Дом: [Роутеры: Домашний]]"
        );
        assert_eq!(list.connection(&new).map(|c| c.name.as_str()), Some("Домашний"));
        // Та же папка — без изменений.
        let before = list.clone();
        assert_eq!(list.move_into(&new, &path("Дом/Роутеры")), Some(new.clone()));
        assert_eq!(list, before);
        // Папку — на верхний уровень.
        let eu = loc_of(&list, "EU");
        assert_eq!(list.move_into(&eu, &[]), Some(vec![3]));
        assert_eq!(list.folder_path(&[3]), Some(path("EU")));
    }

    #[test]
    fn rename_keeps_subtree_and_merges_with_namesake() {
        let mut list = sample();
        let eu = loc_of(&list, "EU");
        list.set_folder_color(&eu, Some("blue"));
        assert_eq!(list.rename_folder(&eu, "  Европа "), Some(eu.clone()));
        assert_eq!(list.folders()[1..3], [path("Прод/Европа"), path("Прод/Европа/Резерв")]);
        assert_eq!(list.folder_color(&eu), Some("blue"));
        assert_eq!(list.rename_folder(&eu, " "), None);

        // «Тест» → «Прод»: содержимое уходит в конец существующей «Прод».
        let test = loc_of(&list, "Тест");
        assert_eq!(list.rename_folder(&test, "Прод"), Some(vec![1]));
        assert_eq!(
            outline(&list),
            "Домашний, [Прод: web, db, [Европа: fra1, [Резерв: ams]], стенд]"
        );
    }

    #[test]
    fn dissolve_moves_children_up_in_place() {
        let mut list = sample();
        let eu = loc_of(&list, "EU");
        assert!(list.dissolve_folder(&eu));
        assert_eq!(
            outline(&list),
            "Домашний, [Прод: web, db, fra1, [Резерв: ams]], [Тест: стенд]"
        );
        // Вложенная папка с именем соседа сливается с ним.
        list.add(c("y"), &path("Прод/Тест"));
        let prod = loc_of(&list, "Прод");
        assert!(list.dissolve_folder(&prod));
        assert_eq!(
            outline(&list),
            "Домашний, web, db, fra1, [Резерв: ams], [Тест: стенд, y]"
        );
        assert!(!list.dissolve_folder(&[0]), "это подключение");
    }

    #[test]
    fn delete_folder_counts_the_whole_subtree() {
        let mut list = sample();
        let eu = loc_of(&list, "EU");
        assert_eq!(list.delete_folder(&eu), 2);
        assert_eq!(outline(&list), "Домашний, [Прод: web, db], [Тест: стенд]");
        assert_eq!(list.delete_folder(&[0]), 0, "это подключение");
        assert_eq!(list.delete_folder(&[1]), 2);
        assert_eq!(list.delete_folder(&[1]), 1);
        assert_eq!(outline(&list), "Домашний");
    }

    #[test]
    fn folder_colors_are_saved() {
        let mut list = sample();
        let prod = loc_of(&list, "Прод");
        list.set_folder_color(&prod, Some("red"));
        let test = loc_of(&list, "Тест");
        list.set_folder_color(&test, Some("no-such-color"));
        assert_eq!(list.folder_color(&prod), Some("red"));
        assert_eq!(list.folder_color(&test), None);
        let restored = Connections::parse(&list.serialize()).expect("parse");
        assert_eq!(restored.folder_color(&prod), Some("red"));
        list.set_folder_color(&prod, None);
        assert_eq!(list.folder_color(&prod), None);
    }

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("connections.ini");
        assert_eq!(Connections::load(&path), Ok(Connections::default()));
        let list = sample();
        list.save(&path).expect("save");
        assert_eq!(Connections::load(&path), Ok(list));
        assert!(!path.with_extension("ini.tmp").exists());
    }
}
