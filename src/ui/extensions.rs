//! Окно «Расширения СУБД»: установленные расширения, установка из
//! «Загрузок», из файла, перетаскиванием или по ссылке, удаление.
//!
//! Сама установка (распаковка, проверка, замена старой версии) — в
//! `db::extension::install`; здесь она только запускается в фоне.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, SystemTime};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::dialogs;
use crate::db::{self, extension::ExtensionInfo};

/// Откуда ставить.
#[derive(Clone)]
pub enum Source {
    File(PathBuf),
    Url(String),
}

/// Устанавливает расширение в фоне; `done` вызывается в главном потоке.
pub fn install(source: Source, done: impl FnOnce(Result<ExtensionInfo, String>) + 'static) {
    let job = db::spawn(async move {
        tokio::task::spawn_blocking(move || match source {
            Source::File(path) => db::extension::install_file(&path),
            Source::Url(url) => db::extension::install_url(&url),
        })
        .await
        .unwrap_or_else(|err| Err(tr!("Installation was interrupted: {}", err)))
    });
    glib::spawn_future_local(async move {
        done(
            job.recv()
                .await
                .unwrap_or_else(|_| Err(tr!("Background tasks did not start").to_string())),
        );
    });
}

/// Следит за «Загрузками»: `on_change` вызывается (с задержкой, чтобы
/// браузер успел дописать файл), когда там что-то появилось или
/// изменилось. Слежение идёт, пока жив возвращённый монитор.
pub fn watch_downloads(on_change: impl Fn() + 'static) -> Option<gio::FileMonitor> {
    let dir = db::extension::downloads_dir()?;
    let monitor = gio::File::for_path(dir)
        .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        .ok()?;
    let pending = Rc::new(Cell::new(false));
    let on_change = Rc::new(on_change);
    monitor.connect_changed(move |_, _, _, _| {
        if pending.replace(true) {
            return;
        }
        let pending = pending.clone();
        let on_change = on_change.clone();
        glib::timeout_add_local_once(Duration::from_millis(700), move || {
            pending.set(false);
            on_change();
        });
    });
    Some(monitor)
}

/// «скачано 5 мин назад, папка «Загрузки»».
pub fn age(modified: Option<SystemTime>) -> String {
    let Some(secs) = modified
        .and_then(|time| SystemTime::now().duration_since(time).ok())
        .map(|since| since.as_secs())
    else {
        return tr!("in the Downloads folder").to_string();
    };
    let when = match secs {
        0..60 => tr!("just now").to_string(),
        60..3600 => tr!("{} min ago", secs / 60),
        3600..86400 => trn!("{} hour ago", "{} hours ago", secs / 3600),
        _ => trn!("{} day ago", "{} days ago", secs / 86400),
    };
    tr!("downloaded {}, Downloads folder", when)
}

/// Путь с `~` вместо домашнего каталога.
pub fn short_path(path: &Path) -> String {
    match path.strip_prefix(glib::home_dir()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

type ChangeHandler = Box<dyn Fn(Option<&ExtensionInfo>)>;

struct Manager {
    me: Weak<Manager>,
    dialog: adw::Dialog,
    toasts: adw::ToastOverlay,
    page: adw::PreferencesPage,
    spinner: adw::Spinner,
    offers_group: adw::PreferencesGroup,
    offers: gtk::ListBox,
    installed: gtk::ListBox,
    url: adw::EntryRow,
    busy: Cell<bool>,
    /// После установки (с новым расширением) и удаления (`None`).
    on_change: ChangeHandler,
    watch: RefCell<Option<gio::FileMonitor>>,
}

/// Открывает окно расширений.
pub fn present(
    parent: &impl IsA<gtk::Widget>,
    on_change: impl Fn(Option<&ExtensionInfo>) + 'static,
) {
    let manager = Manager::new(Box::new(on_change));
    manager.refresh();
    manager.dialog.present(Some(parent));
    // Список мог устареть (расширение положили в папку руками).
    let scan = db::scan_extensions();
    let weak = manager.me.clone();
    glib::spawn_future_local(async move {
        if scan.recv().await.is_ok()
            && let Some(manager) = weak.upgrade()
        {
            manager.refresh_installed();
        }
    });
    // Окно держит менеджер, пока открыто.
    let keep = RefCell::new(Some(manager.clone()));
    manager.dialog.connect_closed(move |_| {
        keep.borrow_mut().take();
    });
}

fn boxed_list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

impl Manager {
    fn new(on_change: ChangeHandler) -> Rc<Self> {
        let page = adw::PreferencesPage::new();

        let offers_group = adw::PreferencesGroup::builder()
            .title(tr!("Downloaded"))
            .description(tr!("Found in the Downloads folder. Installing takes one click."))
            .visible(false)
            .build();
        let offers = boxed_list();
        offers_group.add(&offers);
        page.add(&offers_group);

        let installed_group = adw::PreferencesGroup::builder()
            .title(tr!("Installed"))
            .build();
        let installed = boxed_list();
        installed_group.add(&installed);
        page.add(&installed_group);

        let add_group = adw::PreferencesGroup::builder()
            .title(tr!("Install"))
            .description(tr!(
                "Just download the extension file — sshdeck will notice it in the Downloads \
                 folder. You can also drag a file or archive into this window. An extension \
                 is a program: install only from sources you trust."
            ))
            .build();
        let pick = adw::ActionRow::builder()
            .title(tr!("Choose file…"))
            .subtitle(tr!("Extension program or a .zip or .tar.gz archive"))
            .activatable(true)
            .build();
        pick.add_prefix(&gtk::Image::from_icon_name("document-open-symbolic"));
        pick.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        add_group.add(&pick);
        let url = adw::EntryRow::builder()
            .title(tr!("Link to extension file"))
            .show_apply_button(true)
            .input_purpose(gtk::InputPurpose::Url)
            .build();
        url.add_prefix(&gtk::Image::from_icon_name("web-browser-symbolic"));
        add_group.add(&url);
        let folder = adw::ActionRow::builder()
            .title(tr!("Extensions folder"))
            .subtitle(short_path(&db::extension::drivers_dir()))
            .activatable(true)
            .build();
        folder.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
        folder.add_suffix(&gtk::Image::from_icon_name("folder-open-symbolic"));
        add_group.add(&folder);
        page.add(&add_group);

        let spinner = adw::Spinner::builder().visible(false).build();
        let header = adw::HeaderBar::new();
        header.pack_end(&spinner);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&page));
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&view));
        let dialog = adw::Dialog::builder()
            .title(tr!("Database Extensions"))
            .content_width(520)
            .content_height(600)
            .child(&toasts)
            .build();

        let manager = Rc::new_cyclic(|me| Self {
            me: me.clone(),
            dialog,
            toasts,
            page,
            spinner,
            offers_group,
            offers,
            installed,
            url,
            busy: Cell::new(false),
            on_change,
            watch: RefCell::new(None),
        });

        {
            let weak = manager.me.clone();
            pick.connect_activated(move |_| {
                if let Some(manager) = weak.upgrade() {
                    manager.pick_file();
                }
            });
        }
        {
            let weak = manager.me.clone();
            manager.url.connect_apply(move |row| {
                let text = row.text().trim().to_string();
                if let Some(manager) = weak.upgrade()
                    && !text.is_empty()
                {
                    manager.install(Source::Url(text));
                }
            });
        }
        folder.connect_activated(|_| {
            let dir = db::extension::drivers_dir();
            let _ = std::fs::create_dir_all(&dir);
            let uri = gio::File::for_path(&dir).uri();
            let _ = gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE);
        });
        {
            // Перетаскивание файлов и папок в окно.
            let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
            let weak = manager.me.clone();
            drop.connect_drop(move |_, value, _, _| {
                let (Some(manager), Ok(files)) = (weak.upgrade(), value.get::<gdk::FileList>())
                else {
                    return false;
                };
                let paths: Vec<PathBuf> = files
                    .files()
                    .iter()
                    .filter_map(|file| file.path())
                    .collect();
                if paths.is_empty() {
                    return false;
                }
                manager.install_all(paths);
                true
            });
            manager.toasts.add_controller(drop);
        }
        {
            let weak = manager.me.clone();
            *manager.watch.borrow_mut() = watch_downloads(move || {
                if let Some(manager) = weak.upgrade() {
                    manager.refresh_offers();
                }
            });
        }
        manager
    }

    fn refresh(&self) {
        self.refresh_offers();
        self.refresh_installed();
    }

    fn refresh_offers(&self) {
        self.offers.remove_all();
        let offers = db::extension::download_offers();
        self.offers_group.set_visible(!offers.is_empty());
        for offer in offers {
            let row = adw::ActionRow::builder()
                .title(offer.file_name())
                .subtitle(age(offer.modified))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("folder-download-symbolic"));
            let install = gtk::Button::builder()
                .label(tr!("Install"))
                .valign(gtk::Align::Center)
                .css_classes(["suggested-action", "pill"])
                .build();
            let dismiss = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text(tr!("Dismiss"))
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular"])
                .build();
            {
                let weak = self.me.clone();
                let path = offer.path.clone();
                install.connect_clicked(move |_| {
                    if let Some(manager) = weak.upgrade() {
                        manager.install(Source::File(path.clone()));
                    }
                });
            }
            {
                let weak = self.me.clone();
                let path = offer.path.clone();
                dismiss.connect_clicked(move |_| {
                    db::extension::dismiss_offer(&path);
                    if let Some(manager) = weak.upgrade() {
                        manager.refresh_offers();
                    }
                });
            }
            row.add_suffix(&install);
            row.add_suffix(&dismiss);
            self.offers.append(&row);
        }
    }

    fn refresh_installed(&self) {
        self.installed.remove_all();
        let list = db::extension::installed();
        if list.is_empty() {
            let row = adw::ActionRow::builder()
                .title(tr!("Nothing installed yet"))
                .subtitle(tr!("Built-in PostgreSQL, MySQL/MariaDB and SQLite work without extensions"))
                .css_classes(["dim-label"])
                .build();
            self.installed.append(&row);
        }
        for info in list {
            let row = adw::ActionRow::builder()
                .title(&info.name)
                .subtitle(short_path(&info.path))
                .subtitle_lines(2)
                .build();
            row.add_prefix(
                &gtk::Label::builder()
                    .label(&info.badge)
                    .valign(gtk::Align::Center)
                    .css_classes(["sshdeck-db-badge", "sshdeck-db-extension"])
                    .build(),
            );
            if db::extension::is_removable(&info) {
                let remove = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .tooltip_text(tr!("Remove extension"))
                    .valign(gtk::Align::Center)
                    .css_classes(["flat", "circular"])
                    .build();
                let weak = self.me.clone();
                remove.connect_clicked(move |_| {
                    if let Some(manager) = weak.upgrade() {
                        manager.ask_uninstall(info.clone());
                    }
                });
                row.add_suffix(&remove);
            } else {
                row.add_suffix(
                    &gtk::Label::builder()
                        .label(tr!("system-wide"))
                        .css_classes(["dim-label", "caption"])
                        .build(),
                );
            }
            self.installed.append(&row);
        }
    }

    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.spinner.set_visible(busy);
        self.page.set_sensitive(!busy);
    }

    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    fn pick_file(&self) {
        let dialog = gtk::FileDialog::builder()
            .title(tr!("Extension File"))
            .modal(true)
            .build();
        if let Some(downloads) = db::extension::downloads_dir() {
            dialog.set_initial_folder(Some(&gio::File::for_path(downloads)));
        }
        let weak = self.me.clone();
        dialog.open(
            super::parent_window(&self.dialog).as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                if let (Some(manager), Ok(file)) = (weak.upgrade(), result)
                    && let Some(path) = file.path()
                {
                    manager.install(Source::File(path));
                }
            },
        );
    }

    /// Ставит файлы по очереди (перетащили несколько).
    fn install_all(&self, mut paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let first = paths.remove(0);
        let weak = self.me.clone();
        self.install_then(Source::File(first), move || {
            if let Some(manager) = weak.upgrade() {
                manager.install_all(paths);
            }
        });
    }

    fn install(&self, source: Source) {
        self.install_then(source, || {});
    }

    fn install_then(&self, source: Source, next: impl FnOnce() + 'static) {
        if self.busy.get() {
            return;
        }
        self.set_busy(true);
        let from_url = matches!(source, Source::Url(_));
        let weak = self.me.clone();
        install(source, move |result| {
            let Some(manager) = weak.upgrade() else {
                return;
            };
            manager.set_busy(false);
            manager.refresh();
            match result {
                Ok(info) => {
                    if from_url {
                        manager.url.set_text("");
                    }
                    manager.toast(&tr!("Installed: {}", info.name));
                    (manager.on_change)(Some(&info));
                    next();
                }
                Err(error) => {
                    dialogs::message(&manager.dialog, tr!("Could Not Install Extension"), &error);
                }
            }
        });
    }

    fn ask_uninstall(&self, info: ExtensionInfo) {
        let weak = self.me.clone();
        dialogs::confirm(
            &self.dialog,
            &tr!("Remove Extension {}?", info.name),
            tr!(
                "Saved connections will be kept and can be opened once the extension \
                 is installed again."
            ),
            tr!("Remove"),
            true,
            move || {
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                match db::extension::uninstall(&info) {
                    Ok(()) => {
                        manager.toast(&tr!("Removed: {}", info.name));
                        (manager.on_change)(None);
                    }
                    Err(error) => {
                        dialogs::message(&manager.dialog, tr!("Could Not Remove Extension"), &error);
                    }
                }
                manager.refresh();
            },
        );
    }
}
