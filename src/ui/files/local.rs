//! Дерево этого компьютера: чтение каталогов в фоновых потоках, последняя
//! папка для адреса вкладки и операции с локальными файлами (открыть,
//! показать в файловом менеджере, создать каталог, переименовать,
//! отправить в корзину, скопировать путь).
//!
//! Файловая система может зависнуть (сетевые точки монтирования), поэтому
//! всё, что её трогает, идёт через `gio::spawn_blocking`; ответы на
//! устаревшие переходы отбрасываются по номеру перехода.

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};

use adw::prelude::*;
use gtk::{gio, glib};

use super::tree::{FileTree, Side, TreeRequest, top_level};
use super::{Inner, add_action, add_tree_shortcuts, attach_drop_target, track_active_side};
use super::{dialogs, summarize};
use crate::localdirs::{self, LocalFolders};
use crate::localfs;
use crate::sftp::{self, EntryKind, FileEntry};

/// Чтение и запись файла последних папок разными вкладками по очереди.
static FOLDERS_LOCK: Mutex<()> = Mutex::new(());

/// Сколько ждать ответа файлового менеджера по D-Bus, мс.
const FILE_MANAGER_TIMEOUT_MS: i32 = 5000;

pub(super) struct LocalSide {
    pub(super) tree: FileTree,
    /// Строка навигации и дерево.
    pub(super) section: gtk::Box,
    /// Номер последнего перехода: ответы на прежние отбрасываются.
    nav_seq: Cell<u64>,
    /// Папка, которую ещё предстоит записать как последнюю для адреса.
    unsaved: Arc<Mutex<Option<PathBuf>>>,
    /// Об ошибке записи последней папки уже сказано.
    save_failed: Cell<bool>,
}

impl LocalSide {
    fn next_seq(&self) -> u64 {
        let seq = self.nav_seq.get() + 1;
        self.nav_seq.set(seq);
        seq
    }
}

type Listing = io::Result<(PathBuf, Vec<FileEntry>)>;

/// Создание или переименование; внутри — новое имя.
enum LocalOp {
    Create(String),
    Rename(String),
}

impl LocalOp {
    fn error(&self, err: &io::Error) -> String {
        match self {
            Self::Create(name) | Self::Rename(name)
                if err.kind() == io::ErrorKind::AlreadyExists =>
            {
                tr!("“{}” already exists", name)
            }
            Self::Create(name) => tr!("Could not create “{}”: {}", name, io_message(err)),
            Self::Rename(name) => tr!("Could not rename to “{}”: {}", name, io_message(err)),
        }
    }
}

/// С какой папки открыть локальное дерево вкладки с адресом `address`.
fn start_dir(
    folders: &LocalFolders,
    address: &str,
    home: &Path,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    localdirs::restore_dir(folders.get(address).as_deref(), home, exists)
}

/// Текст ошибки ввода-вывода без хвоста « (os error N)».
fn io_message(err: &io::Error) -> String {
    let text = err.to_string();
    match text.rfind(" (os error ") {
        Some(at) if text.ends_with(')') => text[..at].to_string(),
        _ => text,
    }
}

/// Проверяет имя нового или переименованного элемента.
fn check_name(name: &str) -> Result<(), &'static str> {
    if name.contains('/') {
        Err(tr!("The name cannot contain “/”"))
    } else if name.is_empty() || name == "." || name == ".." {
        Err(tr!("Enter a name"))
    } else {
        Ok(())
    }
}

/// Каталог, который показать в файловом менеджере, если выделенное
/// показать не удалось: каталог первого выделенного элемента или текущий.
fn folder_to_show(selected: &[FileEntry], cwd: &Path) -> PathBuf {
    match selected.first() {
        Some(entry) => sftp::parent_remote(&entry.path),
        None => cwd.to_path_buf(),
    }
}

impl Inner {
    /// Дерево этого компьютера; при первом вызове создаётся и открывает
    /// последнюю папку адреса вкладки.
    pub(super) fn ensure_local(self: &Rc<Self>) -> &LocalSide {
        if let Some(local) = self.local.get() {
            return local;
        }
        let local = self.local.get_or_init(|| self.build_local());
        self.paned.set_start_child(Some(&local.section));
        // Поровну, если панель уже размещена; иначе решает сам Paned.
        let height = self.paned.height();
        if height > 0 {
            self.paned.set_position(height / 2);
        }
        self.start_local();
        local
    }

    fn build_local(self: &Rc<Self>) -> LocalSide {
        let weak = Rc::downgrade(self);
        let menu = gio::Menu::new();
        let open_section = gio::Menu::new();
        open_section.append(Some(tr!("Open")), Some("local.open"));
        open_section.append(Some(tr!("Upload to Server")), Some("local.copy-other"));
        open_section.append(
            Some(tr!("Open in File Manager")),
            Some("local.file-manager"),
        );
        open_section.append(Some(tr!("Copy Path")), Some("local.copy-path"));
        menu.append_section(None, &open_section);
        let create_section = gio::Menu::new();
        create_section.append(Some(tr!("New Folder…")), Some("local.mkdir"));
        menu.append_section(None, &create_section);
        let change_section = gio::Menu::new();
        change_section.append(Some(tr!("Rename…")), Some("local.rename"));
        change_section.append(Some(tr!("Move to Trash")), Some("local.trash"));
        menu.append_section(None, &change_section);
        let refresh_section = gio::Menu::new();
        refresh_section.append(Some(tr!("Refresh")), Some("local.refresh"));
        menu.append_section(None, &refresh_section);

        let tree = FileTree::new(Side::Local, menu.upcast_ref());
        tree.set_home(glib::home_dir());
        tree.set_location_icon(Some(("computer-symbolic", tr!("Files on this computer"))));
        // Дерево пустое, пока не придёт первый каталог: обычно это мгновенно.
        tree.show_files();

        let hidden_toggle = gtk::ToggleButton::builder()
            .icon_name("view-reveal-symbolic")
            .tooltip_text(tr!("Show hidden files"))
            .build();
        tree.nav_row().append(&hidden_toggle);
        let nav_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        nav_box.add_css_class("sshdeck-files-toolbar");
        nav_box.append(tree.nav_row());
        let section = gtk::Box::new(gtk::Orientation::Vertical, 0);
        section.add_css_class("sshdeck-files-section");
        section.append(&nav_box);
        section.append(tree.content());

        let actions = gio::SimpleActionGroup::new();
        add_action(&actions, "open", &weak, |inner| inner.open_local_selected());
        add_action(&actions, "copy-other", &weak, |inner| {
            inner.copy_to_other(Side::Local)
        });
        add_action(&actions, "file-manager", &weak, |inner| {
            inner.show_in_file_manager()
        });
        add_action(&actions, "copy-path", &weak, |inner| {
            inner.copy_local_path()
        });
        add_action(&actions, "mkdir", &weak, |inner| inner.make_local_dir());
        add_action(&actions, "rename", &weak, |inner| inner.rename_local());
        add_action(&actions, "trash", &weak, |inner| inner.trash_local());
        add_action(&actions, "refresh", &weak, |inner| {
            inner.refresh_tree(Side::Local)
        });
        section.insert_action_group("local", Some(&actions));
        add_tree_shortcuts(
            tree.view(),
            &weak,
            Side::Local,
            "local.rename",
            "local.trash",
        );

        {
            let weak = weak.clone();
            tree.set_request_handler(move |side, request| {
                if let Some(inner) = weak.upgrade() {
                    inner.tree_request(side, request);
                }
            });
        }
        {
            let weak = weak.clone();
            tree.set_activate_handler(move |entry| {
                if let Some(inner) = weak.upgrade() {
                    inner.activate_local(entry);
                }
            });
        }
        {
            let weak = weak.clone();
            tree.set_drag_handler(move || weak.upgrade()?.drag_content(Side::Local));
        }
        {
            let tree = tree.clone();
            hidden_toggle.connect_toggled(move |toggle| tree.set_show_hidden(toggle.is_active()));
        }
        track_active_side(&section, &weak, Side::Local);
        attach_drop_target(tree.content(), &weak, Side::Local);

        LocalSide {
            tree,
            section,
            nav_seq: Cell::new(0),
            unsaved: Arc::new(Mutex::new(None)),
            save_failed: Cell::new(false),
        }
    }

    /// Первый каталог: последняя папка адреса, её ближайший существующий
    /// предок, домашняя папка или, в крайнем случае, корень.
    fn start_local(self: &Rc<Self>) {
        let address = self.address.clone();
        let home = glib::home_dir();
        self.local_navigate(None, move || {
            let folders = {
                let _lock = FOLDERS_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
                LocalFolders::load()
            };
            let start = start_dir(&folders, &address, &home, Path::is_dir);
            let mut last_error = io::Error::from(io::ErrorKind::NotFound);
            for dir in [start, home, PathBuf::from("/")] {
                match localfs::navigate(&dir, Path::new("/"))
                    .and_then(|dir| Ok((localfs::list_dir(&dir)?, dir)))
                {
                    Ok((entries, dir)) => return Ok((dir, entries)),
                    Err(err) => last_error = err,
                }
            }
            Err(last_error)
        });
    }

    /// Запрос локального дерева.
    pub(super) fn local_request(self: &Rc<Self>, request: TreeRequest) {
        let Some(local) = self.local.get() else {
            return;
        };
        match request {
            TreeRequest::Navigate(path) => {
                let cwd = local.tree.cwd();
                let requested = path.clone();
                self.local_navigate(Some(requested), move || {
                    let dir = localfs::navigate(&path, &cwd)?;
                    let entries = localfs::list_dir(&dir)?;
                    Ok((dir, entries))
                });
            }
            TreeRequest::List(path) => {
                let weak = Rc::downgrade(self);
                glib::spawn_future_local(async move {
                    let dir = path.clone();
                    // Каталога больше нет (удалён снаружи) — заодно ищется,
                    // куда отступить.
                    let result = gio::spawn_blocking(move || {
                        localfs::list_dir(&dir).map_err(|err| {
                            let fallback = (err.kind() == io::ErrorKind::NotFound)
                                .then(|| localfs::nearest_existing(&dir))
                                .flatten();
                            (err, fallback)
                        })
                    })
                    .await;
                    let Some(inner) = weak.upgrade() else { return };
                    let Some(local) = inner.local.get() else {
                        return;
                    };
                    match result {
                        Ok(Ok(entries)) => local.tree.on_listing(&path, entries),
                        // Пропал показанный каталог: дерево переходит к
                        // ближайшему существующему предку.
                        Ok(Err((_, Some(fallback)))) if path == local.tree.cwd() => {
                            local.tree.navigate(&fallback);
                        }
                        Ok(Err((err, _))) => inner.notifier.error(&tr!(
                            "Could not read “{}”: {}",
                            sftp::display(&path),
                            io_message(&err)
                        )),
                        Err(_) => {}
                    }
                });
            }
        }
    }

    /// Переход в фоновом потоке. `requested` — куда просили перейти (для
    /// сообщения об ошибке); `None` — первый каталог дерева: он не
    /// запоминается, а ошибка показывается вместо дерева.
    fn local_navigate(
        self: &Rc<Self>,
        requested: Option<PathBuf>,
        job: impl FnOnce() -> Listing + Send + 'static,
    ) {
        let Some(local) = self.local.get() else {
            return;
        };
        let seq = local.next_seq();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(job).await;
            let Some(inner) = weak.upgrade() else { return };
            let Some(local) = inner.local.get() else {
                return;
            };
            // Пока читали, попросили перейти ещё раз: этот ответ устарел.
            if local.nav_seq.get() != seq {
                return;
            }
            let error = match result {
                Ok(Ok((dir, entries))) => {
                    local.tree.show_files();
                    local.tree.show_dir(dir.clone(), entries);
                    if requested.is_some() {
                        inner.remember_local(dir);
                    }
                    return;
                }
                Ok(Err(err)) => io_message(&err),
                Err(_) => tr!("internal error").to_string(),
            };
            match requested {
                Some(path) => {
                    inner.notifier.error(&tr!(
                        "Could not open “{}”: {}",
                        sftp::display(&path),
                        error
                    ));
                    local.tree.navigate_failed();
                }
                None => local
                    .tree
                    .show_status(false, tr!("Could Not Open a Local Folder"), &error),
            }
        });
    }

    /// Запоминает папку как последнюю для адреса вкладки. Пишет фоновый
    /// поток; если переходы идут быстрее записи, пишется только последний.
    fn remember_local(self: &Rc<Self>, dir: PathBuf) {
        let Some(local) = self.local.get() else {
            return;
        };
        *local.unsaved.lock().unwrap_or_else(PoisonError::into_inner) = Some(dir);
        let unsaved = Arc::clone(&local.unsaved);
        let address = self.address.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                let _lock = FOLDERS_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
                let Some(dir) = unsaved
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
                else {
                    return Ok(());
                };
                let mut folders = LocalFolders::load();
                folders.remember(&address, &dir);
                folders.save()
            })
            .await;
            let (Some(inner), Ok(Err(err))) = (weak.upgrade(), result) else {
                return;
            };
            // Об ошибке — один раз: дальше каждый переход сообщал бы снова.
            if let Some(local) = inner.local.get()
                && !local.save_failed.replace(true)
            {
                inner
                    .notifier
                    .error(&tr!("Could not remember the local folder: {}", err));
            }
        });
    }

    // --- операции

    fn local_tree(&self) -> Option<&FileTree> {
        self.local.get().map(|local| &local.tree)
    }

    fn activate_local(&self, entry: FileEntry) {
        let Some(tree) = self.local_tree() else {
            return;
        };
        match entry.kind {
            EntryKind::Dir => tree.navigate(&entry.path),
            EntryKind::File => self.open_local(&entry.path),
            EntryKind::Other => self.notifier.info(&tr!(
                "“{}” cannot be opened: it is neither a file nor a folder",
                entry.display_name()
            )),
        }
    }

    fn open_local_selected(&self) {
        let Some(tree) = self.local_tree() else {
            return;
        };
        match tree.selected().as_slice() {
            [entry] => self.activate_local(entry.clone()),
            _ => self.notifier.info(tr!("Select a single file or folder")),
        }
    }

    /// Показывает выделенное в файловом менеджере (по D-Bus,
    /// `org.freedesktop.FileManager1`), а если менеджер не отвечает —
    /// открывает каталог приложением по умолчанию.
    fn show_in_file_manager(self: &Rc<Self>) {
        let Some(tree) = self.local_tree() else {
            return;
        };
        let selected = top_level(tree.selected());
        let cwd = tree.cwd();
        if selected.is_empty() && cwd.as_os_str().is_empty() {
            return;
        }
        let folder = folder_to_show(&selected, &cwd);
        let (method, paths): (&str, Vec<PathBuf>) = if selected.is_empty() {
            ("ShowFolders", vec![cwd])
        } else {
            ("ShowItems", selected.into_iter().map(|e| e.path).collect())
        };
        let uris: Vec<String> = paths
            .iter()
            .filter_map(|path| glib::filename_to_uri(path, None).ok())
            .map(String::from)
            .collect();
        let context = self.root.display().app_launch_context();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            if show_items_over_dbus(method, uris).await.is_ok() {
                return;
            }
            let result = match glib::filename_to_uri(&folder, None) {
                Ok(uri) => gio::AppInfo::launch_default_for_uri_future(&uri, Some(&context)).await,
                Err(err) => Err(err),
            };
            if let (Err(err), Some(inner)) = (result, weak.upgrade()) {
                inner
                    .notifier
                    .error(&tr!("Could not open the file manager: {}", err.message()));
            }
        });
    }

    fn copy_local_path(&self) {
        let Some(tree) = self.local_tree() else {
            return;
        };
        let path = match tree.selected().as_slice() {
            [entry] => entry.path.clone(),
            _ => tree.cwd(),
        };
        let text = sftp::display(&path);
        self.root.clipboard().set_text(&text);
        self.notifier.info(&tr!("Copied: {}", text));
    }

    pub(super) fn make_local_dir(self: &Rc<Self>) {
        let Some(tree) = self.local_tree() else {
            return;
        };
        let cwd = tree.cwd();
        if cwd.as_os_str().is_empty() {
            return;
        }
        let weak = Rc::downgrade(self);
        dialogs::prompt_text(
            &self.root,
            tr!("New Folder"),
            &tr!("Name of the new folder in {}", sftp::display(&cwd)),
            "",
            tr!("Create"),
            move |name| {
                let Some(inner) = weak.upgrade() else { return };
                if let Err(message) = check_name(&name) {
                    inner.notifier.error(message);
                    return;
                }
                let path = cwd.join(&name);
                inner.local_op(cwd, LocalOp::Create(name), move || localfs::make_dir(&path));
            },
        );
    }

    fn rename_local(self: &Rc<Self>) {
        let Some(tree) = self.local_tree() else {
            return;
        };
        let selected = tree.selected();
        let [entry] = selected.as_slice() else {
            self.notifier.info(tr!("Select a single item to rename"));
            return;
        };
        let entry = entry.clone();
        let current_name = entry.display_name();
        let current_name_shown = current_name.clone();
        let weak = Rc::downgrade(self);
        dialogs::prompt_text(
            &self.root,
            tr!("Rename"),
            &tr!("New name for “{}”", current_name),
            &current_name,
            tr!("Rename"),
            move |name| {
                let Some(inner) = weak.upgrade() else { return };
                if let Err(message) = check_name(&name) {
                    inner.notifier.error(message);
                    return;
                }
                if name == current_name_shown {
                    return;
                }
                let parent = sftp::parent_remote(&entry.path);
                let to = parent.join(&name);
                let from = entry.path;
                inner.local_op(parent, LocalOp::Rename(name), move || {
                    localfs::rename_no_clobber(&from, &to)
                });
            },
        );
    }

    /// Создание или переименование в фоновом потоке; затем перечитывается
    /// каталог `dir`.
    fn local_op(
        self: &Rc<Self>,
        dir: PathBuf,
        op: LocalOp,
        job: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(job).await;
            let Some(inner) = weak.upgrade() else { return };
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => inner.notifier.error(&op.error(&err)),
                Err(_) => {}
            }
            inner.refresh_side(Side::Local, &dir);
        });
    }

    /// В корзину; если корзины нет (или не вышло), файлы остаются на
    /// месте — насовсем не удаляется ничего.
    fn trash_local(self: &Rc<Self>) {
        let Some(tree) = self.local_tree() else {
            return;
        };
        let entries = top_level(tree.selected());
        if entries.is_empty() {
            self.notifier.info(tr!("Select what to move to the trash"));
            return;
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut moved = Vec::new();
            let mut failed = Vec::new();
            let mut first_error = None;
            let mut dirs = Vec::new();
            for entry in entries {
                let file = gio::File::for_path(&entry.path);
                match file.trash_future(glib::Priority::DEFAULT).await {
                    Ok(()) => moved.push(entry.display_name()),
                    Err(err) => {
                        failed.push(entry.display_name());
                        first_error.get_or_insert_with(|| err.message().to_string());
                    }
                }
                let parent = sftp::parent_remote(&entry.path);
                if !dirs.contains(&parent) {
                    dirs.push(parent);
                }
            }
            let Some(inner) = weak.upgrade() else { return };
            for dir in &dirs {
                inner.refresh_side(Side::Local, dir);
            }
            if let Some(error) = first_error {
                inner.notifier.error(&tr!(
                    "Could not move {} to the trash: {}",
                    summarize(&failed),
                    error
                ));
            } else if !moved.is_empty() {
                inner
                    .notifier
                    .info(&tr!("Moved to the trash: {}", summarize(&moved)));
            }
        });
    }
}

/// `ShowItems` или `ShowFolders` файлового менеджера по D-Bus.
async fn show_items_over_dbus(method: &str, uris: Vec<String>) -> Result<(), glib::Error> {
    let bus = gio::bus_get_future(gio::BusType::Session).await?;
    bus.call_future(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1",
        method,
        Some(&(uris, "").to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        FILE_MANAGER_TIMEOUT_MS,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_entry;
    use super::*;

    #[test]
    fn start_dir_uses_the_address_folder() {
        let mut folders = LocalFolders::default();
        folders.remember("web", Path::new("/work/site"));
        folders.remember("db", Path::new("/work/gone/deeper"));
        let home = Path::new("/home/me");
        let exists = |dir: &Path| matches!(dir.to_str(), Some("/work/site" | "/work" | "/"));
        assert_eq!(
            start_dir(&folders, "web", home, exists),
            PathBuf::from("/work/site")
        );
        // Папки уже нет — ближайший существующий предок.
        assert_eq!(
            start_dir(&folders, "db", home, exists),
            PathBuf::from("/work")
        );
        // Адрес без запомненной папки — домашняя.
        assert_eq!(start_dir(&folders, "new", home, exists), home);
    }

    #[test]
    fn io_message_drops_the_errno() {
        let err = io::Error::from_raw_os_error(libc::ENOENT);
        let text = io_message(&err);
        assert!(!text.contains("os error"), "{text}");
        assert!(!text.is_empty());
        let custom = io::Error::other("broken (really)");
        assert_eq!(io_message(&custom), "broken (really)");
    }

    #[test]
    fn names_are_checked() {
        assert!(check_name("notes").is_ok());
        assert!(check_name(".hidden").is_ok());
        assert!(check_name("a/b").is_err());
        assert!(check_name("").is_err());
        assert!(check_name(".").is_err());
        assert!(check_name("..").is_err());
    }

    #[test]
    fn folder_to_show_prefers_the_selection() {
        let cwd = Path::new("/home/me");
        assert_eq!(folder_to_show(&[], cwd), cwd);
        let selected = [test_entry("/home/me/docs/a.txt", EntryKind::File)];
        assert_eq!(
            folder_to_show(&selected, cwd),
            PathBuf::from("/home/me/docs")
        );
    }
}
