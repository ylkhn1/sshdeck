//! Обновления в окне: полоса «Вышла sshdeck X.Y.Z» под шапкой, загрузка
//! с ходом и проверкой подписи, установка и перезапуск.
//!
//! Проверка и установка — в `crate::update`; здесь они только
//! запускаются в фоне (`gio::spawn_blocking`), окно не ждёт сети.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use super::{ANIMATION_MS, Notifier, format_size};
use crate::settings::Settings;
use crate::update::install::{self, PackageResult, Progress};
use crate::update::{self, Endpoints, Method, Offer, Version};

/// Первая проверка — через столько секунд после запуска.
const STARTUP_DELAY_SECS: u32 = 8;
/// Как часто смотреть, не пора ли проверить снова (сама проверка — раз
/// в сутки, см. `update::check_due`).
const TICK_SECS: u32 = 60 * 60;
/// Команды обновления сборки из PKGBUILD (в клоне репозитория).
const AUR_COMMAND: &str = "git pull && cd packaging/aur && makepkg -si";

/// Что сейчас показывает полоса.
#[derive(Clone)]
enum State {
    Hidden,
    Offer(Offer),
    /// Новая версия установлена, нужен перезапуск.
    Installed(Version),
}

pub struct Updater {
    window: glib::WeakRef<adw::ApplicationWindow>,
    settings: Rc<RefCell<Settings>>,
    notifier: Notifier,
    revealer: gtk::Revealer,
    title: gtk::Label,
    action: gtk::Button,
    more: gtk::MenuButton,
    state: RefCell<State>,
    /// Установленная в этом запуске версия: о ней и более старых больше
    /// не сообщаем.
    installed: Cell<Option<Version>>,
    checking: Cell<bool>,
    busy: Cell<bool>,
    endpoints: Endpoints,
    method: Method,
    /// Есть ли открытые вкладки (перезапуск их закроет).
    has_sessions: RefCell<Box<dyn Fn() -> bool>>,
}

impl Updater {
    pub fn new(
        window: &adw::ApplicationWindow,
        settings: Rc<RefCell<Settings>>,
        notifier: Notifier,
    ) -> Rc<Self> {
        let icon = gtk::Image::from_icon_name("software-update-available-symbolic");
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let action = gtk::Button::builder()
            .css_classes(["suggested-action"])
            .valign(gtk::Align::Center)
            .build();
        let menu = gio::Menu::new();
        menu.append(Some(tr!("What’s New")), Some("update.whats-new"));
        menu.append(Some(tr!("Skip This Version")), Some("update.skip"));
        let more = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text(tr!("More"))
            .menu_model(&menu)
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text(tr!("Later"))
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        bar.add_css_class("sshdeck-update-bar");
        bar.append(&icon);
        bar.append(&title);
        bar.append(&action);
        bar.append(&more);
        bar.append(&close);
        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .transition_duration(ANIMATION_MS)
            .child(&bar)
            .build();

        let this = Rc::new(Self {
            window: window.downgrade(),
            settings,
            notifier,
            revealer,
            title,
            action,
            more,
            state: RefCell::new(State::Hidden),
            installed: Cell::new(None),
            checking: Cell::new(false),
            busy: Cell::new(false),
            endpoints: Endpoints::current(),
            method: update::this_method(),
            has_sessions: RefCell::new(Box::new(|| false)),
        });

        let group = gio::SimpleActionGroup::new();
        {
            let weak = Rc::downgrade(&this);
            let action = gio::SimpleAction::new("whats-new", None);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade()
                    && let Some(version) = this.offered_version()
                {
                    this.open_release_page(version);
                }
            });
            group.add_action(&action);
        }
        {
            let weak = Rc::downgrade(&this);
            let action = gio::SimpleAction::new("skip", None);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.skip();
                }
            });
            group.add_action(&action);
        }
        bar.insert_action_group("update", Some(&group));
        {
            let weak = Rc::downgrade(&this);
            this.action.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.on_action();
                }
            });
        }
        {
            let weak = Rc::downgrade(&this);
            close.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.revealer.set_reveal_child(false);
                }
            });
        }
        this
    }

    /// Полоса уведомления — под шапкой окна.
    pub fn widget(&self) -> &gtk::Revealer {
        &self.revealer
    }

    pub fn set_sessions_check(&self, check: impl Fn() -> bool + 'static) {
        *self.has_sessions.borrow_mut() = Box::new(check);
    }

    /// Запускает автоматические проверки: вскоре после запуска и потом
    /// раз в сутки (часы сверяются раз в час).
    pub fn start(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local_once(STARTUP_DELAY_SECS, move || {
            if let Some(this) = weak.upgrade() {
                this.auto_check();
            }
        });
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local(TICK_SECS, move || {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            this.auto_check();
            glib::ControlFlow::Continue
        });
    }

    /// Проверка по расписанию: если включена и с прошлой прошли сутки.
    pub fn auto_check(self: &Rc<Self>) {
        let due = {
            let settings = self.settings.borrow();
            update::check_due(
                settings.update_check,
                settings.update_last_check,
                update::unix_now(),
            )
        };
        if due {
            self.check(false);
        }
    }

    /// Включает или выключает автоматическую проверку.
    pub fn set_automatic(self: &Rc<Self>, enabled: bool) {
        self.settings.borrow_mut().update_check = enabled;
        self.save_settings();
        if enabled {
            self.auto_check();
        }
    }

    /// Проверяет обновления. `manual` — по команде из меню: сообщает и
    /// об отсутствии обновлений, и об ошибках, показывает пропущенную
    /// версию.
    pub fn check(self: &Rc<Self>, manual: bool) {
        if self.checking.replace(true) {
            return;
        }
        let endpoints = self.endpoints.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || update::fetch_latest(&endpoints))
                .await
                .unwrap_or_else(|_| Err(tr!("Background tasks did not start").to_string()));
            let Some(this) = weak.upgrade() else { return };
            this.checking.set(false);
            this.on_checked(result, manual);
        });
    }

    fn on_checked(self: &Rc<Self>, result: Result<Option<update::Release>, String>, manual: bool) {
        let release = match result {
            Ok(release) => release,
            Err(err) => {
                if manual {
                    self.notifier
                        .error(&tr!("Could not check for updates: {}", err));
                } else {
                    eprintln!("sshdeck: update check failed: {err}");
                }
                return;
            }
        };
        self.settings.borrow_mut().update_last_check = update::unix_now();
        self.save_settings();
        let current = self.installed.get().unwrap_or_else(Version::current);
        let skipped = self.settings.borrow().update_skipped.clone();
        match release {
            Some(release) if update::should_offer(release.version, current, &skipped, manual) => {
                self.show_offer(update::offer(&release, self.method.clone()));
            }
            _ if manual => match self.installed.get() {
                Some(version) => self.show_installed(version),
                None => self.message(
                    tr!("You Have the Latest Version"),
                    &tr!(
                        "sshdeck {} is the newest version available.",
                        update::CURRENT
                    ),
                ),
            },
            _ => {}
        }
    }

    fn offered_version(&self) -> Option<Version> {
        match &*self.state.borrow() {
            State::Offer(offer) => Some(offer.version),
            State::Installed(version) => Some(*version),
            State::Hidden => None,
        }
    }

    fn show_offer(&self, offer: Offer) {
        self.title
            .set_label(&tr!("sshdeck {} is available", offer.version));
        self.action.set_label(if offer.method.installs() {
            tr!("Update")
        } else {
            tr!("How to Update")
        });
        self.more.set_visible(true);
        *self.state.borrow_mut() = State::Offer(offer);
        self.revealer.set_reveal_child(true);
    }

    fn show_installed(&self, version: Version) {
        self.title
            .set_label(&tr!("Updated to {} — restart sshdeck to use it", version));
        self.action.set_label(tr!("Restart"));
        self.more.set_visible(false);
        *self.state.borrow_mut() = State::Installed(version);
        self.revealer.set_reveal_child(true);
    }

    fn skip(&self) {
        let Some(version) = self.offered_version() else {
            return;
        };
        self.settings.borrow_mut().update_skipped = version.to_string();
        self.save_settings();
        *self.state.borrow_mut() = State::Hidden;
        self.revealer.set_reveal_child(false);
        self.notifier.info(&tr!(
            "sshdeck {} will not be offered again. “Check for Updates…” still shows it.",
            version
        ));
    }

    fn on_action(self: &Rc<Self>) {
        let state = self.state.borrow().clone();
        match state {
            State::Hidden => {}
            State::Installed(_) => self.restart(),
            State::Offer(offer) => match &offer.method {
                Method::SelfReplace(_) | Method::Package(_) => self.download(offer),
                Method::Aur => self.show_aur_help(offer.version),
                Method::ReleasePage => self.show_release_help(offer.version),
            },
        }
    }

    // --------------------------------------------------------- загрузка

    fn download(self: &Rc<Self>, offer: Offer) {
        if self.busy.replace(true) {
            return;
        }
        let progress = Progress::default();
        let (name, total) = offer.asset.clone().unwrap_or_default();
        let bar = gtk::ProgressBar::builder()
            .show_text(true)
            .width_request(320)
            .build();
        let dialog = adw::AlertDialog::new(
            Some(&tr!("Updating to sshdeck {}", offer.version)),
            Some(&tr!("Downloading {}", name)),
        );
        dialog.add_response("cancel", tr!("Cancel"));
        dialog.set_close_response("cancel");
        dialog.set_extra_child(Some(&bar));
        {
            let cancel = progress.cancel.clone();
            dialog.connect_response(None, move |_, _| cancel.store(true, Ordering::Relaxed));
        }
        if let Some(window) = self.window.upgrade() {
            dialog.present(Some(&window));
        }
        {
            let progress = progress.clone();
            let dialog = dialog.downgrade();
            glib::timeout_add_local(Duration::from_millis(150), move || {
                let Some(dialog) = dialog.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                show_progress(&dialog, &bar, &progress, total);
                glib::ControlFlow::Continue
            });
        }

        let endpoints = self.endpoints.clone();
        let dir = update::downloads_dir();
        let job_offer = offer.clone();
        let job_progress = progress.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result =
                gio::spawn_blocking(move || -> Result<Option<std::path::PathBuf>, String> {
                    let file = install::fetch_verified(
                        &job_offer,
                        &endpoints,
                        &dir,
                        update::verify::RELEASE_KEY,
                        &job_progress,
                    )?;
                    // Отменили, пока проверялась подпись.
                    if job_progress.cancel.load(Ordering::Relaxed) {
                        install::forget_download(&file);
                        return Err(install::CANCELLED.to_string());
                    }
                    if let Method::SelfReplace(exe) = &job_offer.method {
                        job_progress
                            .stage
                            .store(install::STAGE_INSTALL, Ordering::Relaxed);
                        let binary = install::extract_binary(&file, job_offer.version);
                        install::forget_download(&file);
                        install::replace_executable(exe, &binary?)?;
                        return Ok(None);
                    }
                    Ok(Some(file))
                })
                .await
                .unwrap_or_else(|_| Err(tr!("Background tasks did not start").to_string()));
            dialog.force_close();
            let Some(this) = weak.upgrade() else { return };
            this.busy.set(false);
            match result {
                Ok(None) => this.installed_now(offer.version),
                Ok(Some(package)) => this.confirm_package(offer, package),
                Err(err) if err == install::CANCELLED => {}
                Err(err) => this.show_failure(offer.version, &err),
            }
        });
    }

    fn installed_now(&self, version: Version) {
        self.installed.set(Some(version));
        {
            let mut settings = self.settings.borrow_mut();
            if Version::parse(&settings.update_skipped).is_some_and(|skipped| skipped <= version) {
                settings.update_skipped.clear();
            }
        }
        self.save_settings();
        self.show_installed(version);
    }

    /// Пакет скачан и проверен: поставить его через системный запрос
    /// пароля.
    fn confirm_package(self: &Rc<Self>, offer: Offer, package: std::path::PathBuf) {
        let Method::Package(kind) = offer.method else {
            return;
        };
        let dialog = adw::AlertDialog::new(
            Some(&tr!("Install sshdeck {}?", offer.version)),
            Some(tr!(
                "The package is downloaded and its signature is verified. \
                 The system will ask for your password to install it."
            )),
        );
        dialog.add_responses(&[("cancel", tr!("Cancel")), ("install", tr!("Install"))]);
        dialog.set_close_response("cancel");
        dialog.set_default_response(Some("install"));
        dialog.set_response_appearance("install", adw::ResponseAppearance::Suggested);
        let weak = Rc::downgrade(self);
        let parent = self.window.upgrade();
        dialog.choose(parent.as_ref(), gio::Cancellable::NONE, move |response| {
            let Some(this) = weak.upgrade() else { return };
            if response != "install" {
                return;
            }
            if this.busy.replace(true) {
                return;
            }
            this.notifier.info(tr!("Installing the update…"));
            let job_package = package.clone();
            glib::spawn_future_local(async move {
                let result =
                    gio::spawn_blocking(move || install::install_package(kind, &job_package))
                        .await
                        .unwrap_or_else(|_| {
                            PackageResult::Failed(tr!("Background tasks did not start").to_string())
                        });
                this.busy.set(false);
                match result {
                    PackageResult::Installed => {
                        install::forget_download(&package);
                        this.installed_now(offer.version);
                    }
                    PackageResult::Cancelled => this.show_manual_install(
                        kind,
                        &package,
                        tr!("The installation was cancelled."),
                    ),
                    PackageResult::Failed(err) => this.show_manual_install(
                        kind,
                        &package,
                        &tr!("Could not install the package: {}", err),
                    ),
                }
            });
        });
    }

    // ---------------------------------------------------------- диалоги

    fn message(&self, heading: &str, body: &str) {
        if let Some(window) = self.window.upgrade() {
            super::dialogs::message(&window, heading, body);
        }
    }

    /// Диалог с командой для терминала и кнопкой «Скопировать».
    fn command_dialog(&self, heading: &str, body: &str, command: &str, page: Option<Version>) {
        // Длинный путь переносится где угодно, но без дефисов: команду
        // копируют как есть.
        let attributes = gtk::pango::AttrList::new();
        attributes.insert(gtk::pango::AttrInt::new_insert_hyphens(false));
        let label = gtk::Label::builder()
            .label(command)
            .selectable(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .attributes(&attributes)
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["sshdeck-update-command"])
            .build();
        let copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text(tr!("Copy"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let command = command.to_string();
            let notifier = self.notifier.clone();
            copy.connect_clicked(move |button| {
                button.clipboard().set_text(&command);
                notifier.info(&tr!("Copied: {}", command));
            });
        }
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&label);
        row.append(&copy);
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.set_extra_child(Some(&row));
        dialog.add_response("close", tr!("Close"));
        dialog.set_close_response("close");
        if page.is_some() {
            dialog.add_response("page", tr!("What’s New"));
        }
        let endpoints = self.endpoints.clone();
        let parent = self.window.upgrade();
        let window = self.window.clone();
        dialog.choose(parent.as_ref(), gio::Cancellable::NONE, move |response| {
            if response == "page"
                && let Some(version) = page
            {
                launch(&window, &endpoints.release_page(version));
            }
        });
    }

    fn show_manual_install(
        &self,
        kind: update::InstallKind,
        package: &std::path::Path,
        reason: &str,
    ) {
        let Some(command) = install::manual_command(kind, package) else {
            return;
        };
        self.command_dialog(
            tr!("Install the Update Manually"),
            &tr!(
                "{}\n\nThe package is downloaded and verified. To install it, run in a terminal:",
                reason
            ),
            &command,
            None,
        );
    }

    fn show_aur_help(&self, version: Version) {
        self.command_dialog(
            tr!("How to Update"),
            &tr!(
                "sshdeck {} is available. This copy was built from the PKGBUILD in the sshdeck \
                 repository. To update, run this in your clone of the repository:",
                version
            ),
            AUR_COMMAND,
            Some(version),
        );
    }

    fn show_release_help(&self, version: Version) {
        let body = tr!(
            "sshdeck {} is available. This copy cannot update itself: download the new version \
             from the release page.",
            version
        );
        self.page_dialog(tr!("How to Update"), &body, version);
    }

    fn show_failure(&self, version: Version, error: &str) {
        let body = tr!(
            "{}\n\nNothing was installed. You can download the new version from the release page.",
            error
        );
        self.page_dialog(tr!("Could Not Update"), &body, version);
    }

    /// Сообщение с кнопкой «Открыть страницу выпуска».
    fn page_dialog(&self, heading: &str, body: &str, version: Version) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_responses(&[("close", tr!("Close")), ("page", tr!("Open Release Page"))]);
        dialog.set_close_response("close");
        dialog.set_default_response(Some("page"));
        dialog.set_response_appearance("page", adw::ResponseAppearance::Suggested);
        let url = self.endpoints.release_page(version);
        let window = self.window.clone();
        let parent = self.window.upgrade();
        dialog.choose(parent.as_ref(), gio::Cancellable::NONE, move |response| {
            if response == "page" {
                launch(&window, &url);
            }
        });
    }

    fn open_release_page(&self, version: Version) {
        launch(&self.window, &self.endpoints.release_page(version));
    }

    // -------------------------------------------------------- перезапуск

    fn restart(self: &Rc<Self>) {
        if (self.has_sessions.borrow())() {
            let Some(window) = self.window.upgrade() else {
                return;
            };
            let weak = Rc::downgrade(self);
            super::dialogs::confirm(
                &window,
                tr!("Restart sshdeck?"),
                tr!("Open sessions and tabs will be closed. Saved connections stay."),
                tr!("Restart"),
                true,
                move || {
                    if let Some(this) = weak.upgrade() {
                        this.restart_now();
                    }
                },
            );
        } else {
            self.restart_now();
        }
    }

    fn restart_now(&self) {
        let Some(exe) = update::startup_exe() else {
            return;
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        match install::spawn_new(exe, &args) {
            // Закрытие окна завершает сессии и программу.
            Ok(()) => {
                if let Some(window) = self.window.upgrade() {
                    window.close();
                }
            }
            Err(err) => self.notifier.error(&err),
        }
    }

    fn save_settings(&self) {
        if let Err(err) = self.settings.borrow().save() {
            self.notifier
                .error(&tr!("Could not save settings: {}", err));
        }
    }
}

/// Текст и полоса хода в диалоге загрузки.
fn show_progress(
    dialog: &adw::AlertDialog,
    bar: &gtk::ProgressBar,
    progress: &Progress,
    total: u64,
) {
    match progress.stage.load(Ordering::Relaxed) {
        install::STAGE_DOWNLOAD => {
            let done = progress.downloaded.load(Ordering::Relaxed);
            if total > 0 {
                bar.set_fraction((done as f64 / total as f64).min(1.0));
                bar.set_text(Some(&tr!(
                    "{} of {}",
                    format_size(done),
                    format_size(total)
                )));
            } else {
                bar.pulse();
                bar.set_text(Some(&format_size(done)));
            }
        }
        install::STAGE_VERIFY => {
            dialog.set_body(tr!("Checking the signature…"));
            bar.set_fraction(1.0);
            bar.set_show_text(false);
        }
        _ => {
            dialog.set_body(tr!("Installing…"));
            dialog.set_response_enabled("cancel", false);
        }
    }
}

/// Открывает ссылку в браузере.
fn launch(window: &glib::WeakRef<adw::ApplicationWindow>, url: &str) {
    let window = window.upgrade();
    gtk::UriLauncher::new(url).launch(window.as_ref(), gio::Cancellable::NONE, |_| {});
}
