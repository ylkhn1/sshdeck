//! Подключения к базам данных: список сохранённых баз (его показывает
//! левая панель), диалог их настройки и пароли.
//!
//! Пароли хранятся в системном хранилище паролей (KWallet, GNOME Keyring)
//! и в памяти до закрытия программы; на диск приложение их не пишет.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gio, glib};

use super::{Notifier, dialogs, extensions};
use crate::db::{self, DbKind, DbProfile, Profiles, profile, secrets};

const CANCEL: &str = "cancel";
const ACCEPT: &str = "accept";

/// Открывает вкладку базы с уже известным паролем.
type OpenHandler = Box<dyn Fn(&DbProfile, String)>;

/// Квадратный значок СУБД: `PG`, `MY`, `SL`, у расширений — свой.
pub fn profile_badge(profile: &DbProfile) -> gtk::Label {
    gtk::Label::builder()
        .label(profile.badge())
        .valign(gtk::Align::Center)
        .tooltip_text(profile.kind_label())
        .css_classes([
            "sshdeck-db-badge",
            &format!("sshdeck-db-{}", profile.kind.id()),
        ])
        .build()
}

/// Описание профиля одной строкой для списка и подсказок.
pub fn describe(profile: &DbProfile) -> String {
    let mut text = profile.summary();
    if profile.is_networked() && !profile.ssh.is_empty() {
        text.push_str(&tr!(" · via {}", profile.ssh));
    }
    text
}

/// Сохранённые базы данных. Копии разделяют одно состояние.
#[derive(Clone)]
pub struct Databases {
    inner: Rc<Inner>,
}

struct Inner {
    me: Weak<Inner>,
    profiles: RefCell<Profiles>,
    notifier: Notifier,
    /// Над чем показывать диалоги (окно приложения).
    parent: RefCell<Option<gtk::Widget>>,
    open: RefCell<Option<OpenHandler>>,
    /// Вызывается после любого изменения списка.
    changed: RefCell<Vec<Box<dyn Fn()>>>,
    /// Пароли, уже полученные в этом запуске, по ключу `secrets::key`.
    passwords: RefCell<HashMap<String, String>>,
}

impl Databases {
    pub fn new(notifier: Notifier) -> Self {
        let profiles = match Profiles::load(&profile::default_path()) {
            Ok(profiles) => profiles,
            Err(err) => {
                notifier.error(&tr!("Could not read the database list: {}", err));
                Profiles::default()
            }
        };
        let inner = Rc::new_cyclic(|me| Inner {
            me: me.clone(),
            profiles: RefCell::new(profiles),
            notifier,
            parent: RefCell::new(None),
            open: RefCell::new(None),
            changed: RefCell::new(Vec::new()),
            passwords: RefCell::new(HashMap::new()),
        });
        // Расширения ищутся в фоне; когда найдутся, список перерисуется
        // (значки и названия СУБД из расширений).
        let scan = db::scan_extensions();
        let weak = Rc::downgrade(&inner);
        glib::spawn_future_local(async move {
            if scan.recv().await.is_ok()
                && let Some(inner) = weak.upgrade()
            {
                for handler in inner.changed.borrow().iter() {
                    handler();
                }
            }
        });
        Self { inner }
    }

    pub fn set_parent(&self, parent: &impl IsA<gtk::Widget>) {
        *self.inner.parent.borrow_mut() = Some(parent.as_ref().clone());
    }

    /// Как открыть вкладку, когда пароль известен.
    pub fn set_open_handler(&self, handler: impl Fn(&DbProfile, String) + 'static) {
        *self.inner.open.borrow_mut() = Some(Box::new(handler));
    }

    /// Подписка на изменения списка (панель перерисовывается).
    pub fn connect_changed(&self, handler: impl Fn() + 'static) {
        self.inner.changed.borrow_mut().push(Box::new(handler));
    }

    pub fn profiles(&self) -> Vec<DbProfile> {
        self.inner.profiles.borrow().items().to_vec()
    }

    /// Окно расширений СУБД.
    pub fn extensions(&self) {
        let Some(parent) = self.inner.parent() else {
            return;
        };
        let weak = Rc::downgrade(&self.inner);
        extensions::present(&parent, move |_| {
            // Значки и названия СУБД в списке баз берутся из расширений.
            if let Some(inner) = weak.upgrade() {
                for handler in inner.changed.borrow().iter() {
                    handler();
                }
            }
        });
    }

    /// Открывает базу: пароль берётся из памяти, затем из хранилища
    /// паролей, а если его нигде нет — спрашивается.
    pub fn connect(&self, profile: &DbProfile) {
        self.inner.connect(profile);
    }

    /// Спрашивает пароль (например, после отказа сервера) и передаёт его в
    /// `on_password`; при согласии пароль запоминается.
    pub fn ask_password(
        &self,
        profile: &DbProfile,
        reason: Option<&str>,
        on_password: impl FnOnce(String) + 'static,
    ) {
        self.inner.ask_password(profile, reason, on_password);
    }

    pub fn create(&self) {
        self.inner.edit(None, None);
    }

    pub fn edit(&self, profile: &DbProfile) {
        self.inner.edit(Some(profile.clone()), None);
    }

    pub fn duplicate(&self, profile: &DbProfile) {
        let copy = DbProfile {
            name: tr!("{} (copy)", profile.name),
            ..profile.clone()
        };
        self.inner.edit(None, Some(copy));
    }

    pub fn delete(&self, profile: &DbProfile) {
        self.inner.delete(profile.clone());
    }
}

impl Inner {
    fn parent(&self) -> Option<gtk::Widget> {
        self.parent.borrow().clone()
    }

    fn open(&self, profile: &DbProfile, password: String) {
        if let Some(open) = self.open.borrow().as_ref() {
            open(profile, password);
        }
    }

    fn commit(&self) {
        if let Err(err) = self.profiles.borrow().save(&profile::default_path()) {
            self.notifier
                .error(&tr!("Could not save the database list: {}", err));
        }
        for handler in self.changed.borrow().iter() {
            handler();
        }
    }

    fn connect(&self, profile: &DbProfile) {
        if !profile.is_networked() {
            self.open(profile, String::new());
            return;
        }
        let key = secrets::key(profile);
        let cached = self.passwords.borrow().get(&key).cloned();
        if let Some(password) = cached {
            self.open(profile, password);
            return;
        }
        let result = db::spawn(secrets::load(key.clone()));
        let weak = self.me.clone();
        let profile = profile.clone();
        glib::spawn_future_local(async move {
            let result = result.recv().await;
            let Some(this) = weak.upgrade() else { return };
            match result {
                Ok(Ok(Some(password))) => {
                    this.passwords.borrow_mut().insert(key, password.clone());
                    this.open(&profile, password);
                }
                Ok(Err(message)) => {
                    this.notifier.info(&message);
                    this.prompt_and_open(&profile);
                }
                _ => this.prompt_and_open(&profile),
            }
        });
    }

    fn prompt_and_open(&self, profile: &DbProfile) {
        let weak = self.me.clone();
        let target = profile.clone();
        self.ask_password(profile, None, move |password| {
            if let Some(this) = weak.upgrade() {
                this.open(&target, password);
            }
        });
    }

    /// Запоминает пароль в памяти и (по желанию) в хранилище паролей.
    fn remember(&self, profile: &DbProfile, password: &str, keep: bool) {
        let key = secrets::key(profile);
        self.passwords
            .borrow_mut()
            .insert(key.clone(), password.to_string());
        let job = if keep {
            db::spawn(secrets::store(
                key,
                secrets::label(profile),
                password.to_string(),
            ))
        } else {
            // Не запоминать — значит и прежний пароль больше не подставлять.
            db::spawn(secrets::forget(key))
        };
        let weak = self.me.clone();
        glib::spawn_future_local(async move {
            if let Ok(Err(message)) = job.recv().await
                && let Some(this) = weak.upgrade()
                && keep
            {
                this.notifier
                    .error(&tr!("Password not saved: {}", message));
            }
        });
    }

    fn forget(&self, key: String) {
        self.passwords.borrow_mut().remove(&key);
        let job = db::spawn(secrets::forget(key));
        glib::spawn_future_local(async move {
            let _ = job.recv().await;
        });
    }

    fn ask_password(
        &self,
        profile: &DbProfile,
        reason: Option<&str>,
        on_password: impl FnOnce(String) + 'static,
    ) {
        let password_row = adw::PasswordEntryRow::builder()
            .title(tr!("Password"))
            .activates_default(true)
            .build();
        let keep_row = adw::SwitchRow::builder()
            .title(tr!("Remember password"))
            .subtitle(tr!("Stored in the system password storage (KWallet), not in a file"))
            .active(true)
            .build();
        let fields = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .width_request(380)
            .build();
        fields.append(&password_row);
        fields.append(&keep_row);
        let mut body = format!("{} — {}", profile.kind_label(), describe(profile));
        if let Some(reason) = reason {
            body = format!("{reason}\n\n{body}");
        }
        let dialog = adw::AlertDialog::new(
            Some(&tr!("Connect to “{}”", profile.name)),
            Some(&body),
        );
        dialog.add_responses(&[(CANCEL, tr!("Cancel")), (ACCEPT, tr!("Connect"))]);
        dialog.set_close_response(CANCEL);
        dialog.set_default_response(Some(ACCEPT));
        dialog.set_response_appearance(ACCEPT, adw::ResponseAppearance::Suggested);
        dialog.set_extra_child(Some(&fields));
        dialog.set_focus(Some(&password_row));
        let weak = self.me.clone();
        let profile = profile.clone();
        dialog.choose(
            self.parent().as_ref(),
            gio::Cancellable::NONE,
            move |response| {
                let Some(this) = weak.upgrade() else { return };
                if response != ACCEPT {
                    return;
                }
                let password = password_row.text().to_string();
                this.remember(&profile, &password, keep_row.is_active());
                on_password(password);
            },
        );
    }

    fn delete(&self, profile: DbProfile) {
        let Some(parent) = self.parent() else { return };
        let weak = self.me.clone();
        dialogs::confirm(
            &parent,
            tr!("Delete Connection?"),
            &tr!(
                "“{}” will be removed from the database list together with its saved \
                 password. The database itself will not change.",
                profile.name
            ),
            tr!("Delete"),
            true,
            move || {
                let Some(this) = weak.upgrade() else { return };
                this.profiles.borrow_mut().remove(&profile);
                // Пароль общий для подключений к тому же серверу под тем же
                // пользователем: удаляем, только если он больше не нужен.
                let key = secrets::key(&profile);
                let shared = this
                    .profiles
                    .borrow()
                    .items()
                    .iter()
                    .any(|other| secrets::key(other) == key);
                if !shared && profile.is_networked() {
                    this.forget(key);
                }
                this.commit();
            },
        );
    }

    /// Диалог подключения: изменение `original` или новое (из `template`).
    fn edit(&self, original: Option<DbProfile>, template: Option<DbProfile>) {
        let Some(parent) = self.parent() else { return };
        let start = original
            .clone()
            .or(template)
            .unwrap_or_else(|| DbProfile::template(DbKind::Postgres));
        let form = ProfileForm::new(start.clone());
        {
            let weak = self.me.clone();
            *form.on_extensions.borrow_mut() = Some(Box::new(move || {
                if let Some(this) = weak.upgrade() {
                    for handler in this.changed.borrow().iter() {
                        handler();
                    }
                }
            }));
        }
        // Пароль из хранилища подставляется в поле, когда придёт.
        if start.is_networked() {
            let key = secrets::key(&start);
            let cached = self.passwords.borrow().get(&key).cloned();
            match cached {
                Some(password) => form.set_known_password(Some(password)),
                None => {
                    let job = db::spawn(secrets::load(key));
                    let weak = Rc::downgrade(&form);
                    glib::spawn_future_local(async move {
                        let password = job.recv().await.ok().and_then(Result::ok).flatten();
                        if let Some(form) = weak.upgrade() {
                            form.set_known_password(password);
                        }
                    });
                }
            }
        } else {
            form.set_known_password(None);
        }

        let dialog = adw::AlertDialog::new(
            Some(if original.is_some() {
                tr!("Edit Database Connection")
            } else {
                tr!("New Database Connection")
            }),
            Some(tr!(
                "When connecting over SSH, enter the database server as the SSH \
                 server sees it (usually localhost)."
            )),
        );
        dialog.add_responses(&[(CANCEL, tr!("Cancel")), (ACCEPT, tr!("Save"))]);
        dialog.set_close_response(CANCEL);
        dialog.set_default_response(Some(ACCEPT));
        dialog.set_response_appearance(ACCEPT, adw::ResponseAppearance::Suggested);
        dialog.set_extra_child(Some(&form.widget));
        dialog.set_focus(Some(&form.name));
        {
            let weak_dialog = dialog.downgrade();
            form.connect_changed(move |valid| {
                if let Some(dialog) = weak_dialog.upgrade() {
                    dialog.set_response_enabled(ACCEPT, valid);
                }
            });
        }

        let weak = self.me.clone();
        dialog.choose(Some(&parent), gio::Cancellable::NONE, move |response| {
            if response != ACCEPT {
                return;
            }
            let (Some(this), Some(profile)) = (weak.upgrade(), form.profile()) else {
                return;
            };
            this.save_password(original.as_ref(), &profile, &form);
            this.profiles
                .borrow_mut()
                .upsert(profile, original.as_ref());
            this.commit();
        });
    }

    /// Пароль из формы: сохраняется, переносится на новый ключ (если
    /// сменились сервер или пользователь) или удаляется (поле очистили).
    fn save_password(&self, original: Option<&DbProfile>, profile: &DbProfile, form: &ProfileForm) {
        if !profile.is_networked() {
            return;
        }
        let key = secrets::key(profile);
        let old_key = original.filter(|old| old.is_networked()).map(secrets::key);
        let moved = old_key.as_ref().is_some_and(|old| *old != key);
        let Some(password) = form.password_to_save() else {
            // Пароль не трогали и он ещё не загружен — ничего не меняем.
            return;
        };
        if form.password_changed() || moved {
            if password.is_empty() {
                self.forget(key);
            } else {
                self.remember(profile, &password, true);
            }
        }
        if moved && let Some(old) = old_key {
            let still_used = self
                .profiles
                .borrow()
                .items()
                .iter()
                .any(|other| Some(other) != original && secrets::key(other) == old);
            if !still_used {
                self.forget(old);
            }
        }
    }
}

/// Вариант в списке СУБД: встроенный драйвер или расширение.
#[derive(Clone)]
struct KindChoice {
    kind: DbKind,
    driver: String,
    label: String,
}

impl KindChoice {
    fn template(&self) -> DbProfile {
        DbProfile::template_for(self.kind, self.driver.clone())
    }
}

/// Встроенные СУБД, затем установленные расширения (список обновляется при
/// каждом открытии диалога: новое расширение появляется без перезапуска),
/// затем — расширение профиля, если его удалили.
fn kind_choices(current: &DbProfile) -> Vec<KindChoice> {
    let mut choices: Vec<KindChoice> = DbKind::ALL
        .iter()
        .map(|kind| KindChoice {
            kind: *kind,
            driver: String::new(),
            label: kind.label().to_string(),
        })
        .collect();
    for ext in db::extension::scan() {
        choices.push(KindChoice {
            kind: DbKind::Extension,
            label: tr!("{} (extension)", ext.name),
            driver: ext.id,
        });
    }
    if current.kind == DbKind::Extension
        && !choices.iter().any(|choice| choice.driver == current.driver)
    {
        choices.push(KindChoice {
            kind: DbKind::Extension,
            driver: current.driver.clone(),
            label: current.kind_label(),
        });
    }
    choices
}

/// Поля диалога подключения. Набор видимых полей зависит от СУБД.
struct ProfileForm {
    widget: gtk::ListBox,
    kind: adw::ComboRow,
    choices: RefCell<Vec<KindChoice>>,
    /// Строка о расширениях; если в «Загрузках» есть новое — с кнопкой
    /// «Установить».
    ext_row: adw::ActionRow,
    ext_install: gtk::Button,
    offer: RefCell<Option<db::extension::Offer>>,
    downloads_watch: RefCell<Option<gio::FileMonitor>>,
    /// СУБД профиля при открытии: её расширение остаётся в списке, даже
    /// если его удалили.
    start_kind: (DbKind, String),
    /// Порт и пользователь по умолчанию выбранной СУБД. Хранятся
    /// значения, а не шаблон: у удалённого расширения их уже не узнать.
    kind_defaults: RefCell<(u16, String)>,
    /// Вызывается, когда расширения поставили или удалили (панель баз
    /// перерисовывает значки).
    on_extensions: RefCell<Option<Box<dyn Fn()>>>,
    name: adw::EntryRow,
    host: adw::EntryRow,
    port: adw::EntryRow,
    user: adw::EntryRow,
    database: adw::EntryRow,
    ssh: adw::EntryRow,
    tls: adw::SwitchRow,
    path: adw::EntryRow,
    password: adw::PasswordEntryRow,
    /// Пароль из хранилища; `None` внутри — пароля нет. Пока он не
    /// загружен (внешний `None`), поле пароля не трогается при сохранении.
    known_password: RefCell<Option<Option<String>>>,
    /// Пользователь менял поле пароля.
    password_edited: Cell<bool>,
    /// Поле меняется программно (подстановка сохранённого пароля).
    filling_password: Cell<bool>,
}

impl ProfileForm {
    fn new(profile: DbProfile) -> Rc<Self> {
        let choices = kind_choices(&profile);
        let labels: Vec<&str> = choices.iter().map(|choice| choice.label.as_str()).collect();
        let kind = adw::ComboRow::builder()
            .title(tr!("Database type"))
            .model(&gtk::StringList::new(&labels))
            .selected(
                choices
                    .iter()
                    .position(|choice| {
                        choice.kind == profile.kind && choice.driver == profile.driver
                    })
                    .unwrap_or(0) as u32,
            )
            .build();
        let entry = |title: &str, text: &str| {
            adw::EntryRow::builder()
                .title(title)
                .text(text)
                .activates_default(true)
                .build()
        };
        let name = entry(trc!("connection or command", "Name"), &profile.name);
        let host = entry(tr!("Server"), &profile.host);
        let port = entry(tr!("Port"), &profile.port.to_string());
        port.set_input_purpose(gtk::InputPurpose::Digits);
        let user = entry(tr!("User"), &profile.user);
        let database = entry(tr!("Database (optional)"), &profile.database);
        let ssh = entry(tr!("Via SSH server (optional): user@host"), &profile.ssh);
        let tls = adw::SwitchRow::builder()
            .title(tr!("Require encryption (TLS)"))
            .subtitle(tr!("The server certificate is not verified, as with sslmode=require"))
            .active(profile.tls)
            .build();
        let path = entry(tr!("Database file"), &profile.path);
        let password = adw::PasswordEntryRow::builder()
            .title(tr!("Password (stored in KWallet, not in a file)"))
            .activates_default(true)
            .build();

        let widget = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .width_request(420)
            .build();
        let ext_row = adw::ActionRow::builder()
            .title(tr!("Other databases — extensions"))
            .subtitle_lines(3)
            .activatable(true)
            .build();
        let ext_install = gtk::Button::builder()
            .label(tr!("Install"))
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action", "pill"])
            .visible(false)
            .build();
        ext_row.add_suffix(&ext_install);
        ext_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        widget.append(&kind);
        widget.append(&ext_row);
        widget.append(&name);
        widget.append(&host);
        widget.append(&port);
        widget.append(&user);
        widget.append(&password);
        widget.append(&database);
        widget.append(&ssh);
        widget.append(&tls);
        widget.append(&path);

        let form = Rc::new(Self {
            widget,
            kind,
            choices: RefCell::new(choices),
            ext_row,
            ext_install,
            offer: RefCell::new(None),
            downloads_watch: RefCell::new(None),
            start_kind: (profile.kind, profile.driver.clone()),
            kind_defaults: RefCell::new({
                let template = DbProfile::template_for(profile.kind, profile.driver.clone());
                (template.default_port(), template.default_user())
            }),
            on_extensions: RefCell::new(None),
            name,
            host,
            port,
            user,
            database,
            ssh,
            tls,
            path,
            password,
            known_password: RefCell::new(None),
            password_edited: Cell::new(false),
            filling_password: Cell::new(false),
        });
        {
            let weak = Rc::downgrade(&form);
            form.password.connect_changed(move |_| {
                if let Some(form) = weak.upgrade()
                    && !form.filling_password.get()
                {
                    form.password_edited.set(true);
                }
            });
        }

        // Выбор файла SQLite.
        let pick = gtk::Button::builder()
            .icon_name("document-open-symbolic")
            .tooltip_text(tr!("Choose file"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let weak = Rc::downgrade(&form);
            pick.connect_clicked(move |button| {
                let dialog = gtk::FileDialog::builder()
                    .title(tr!("Database File"))
                    .modal(true)
                    .build();
                let weak = weak.clone();
                dialog.open(
                    super::parent_window(button).as_ref(),
                    gio::Cancellable::NONE,
                    move |result| {
                        let (Some(form), Ok(file)) = (weak.upgrade(), result) else {
                            return;
                        };
                        if let Some(path) = file.path() {
                            form.path.set_text(&path.to_string_lossy());
                        }
                    },
                );
            });
        }
        form.path.add_suffix(&pick);

        {
            // Порт и пользователь по умолчанию следуют за СУБД, если их не
            // меняли.
            let weak = Rc::downgrade(&form);
            form.kind.connect_selected_notify(move |_| {
                if let Some(form) = weak.upgrade() {
                    form.kind_switched();
                }
            });
        }
        form.connect_extensions();
        form.sync_visibility();
        form
    }

    /// Порт и пользователь по умолчанию следуют за СУБД, если их не меняли.
    fn kind_switched(&self) {
        let new = self.selected().template();
        let (old_port, old_user) = self
            .kind_defaults
            .replace((new.default_port(), new.default_user()));
        let port = self.port.text();
        if port.trim().is_empty() || port.trim() == old_port.to_string() {
            self.port.set_text(&new.default_port().to_string());
        }
        if self.user.text().trim() == old_user {
            self.user.set_text(&new.default_user());
        }
        if new.is_networked() && self.host.text().trim().is_empty() {
            self.host.set_text("localhost");
        }
        self.sync_visibility();
    }

    fn selected(&self) -> KindChoice {
        let choices = self.choices.borrow();
        choices
            .get(self.kind.selected() as usize)
            .cloned()
            .unwrap_or_else(|| choices[0].clone())
    }

    /// Строка расширений: открывает окно расширений, а если в «Загрузках»
    /// появилось новое — предлагает поставить его прямо отсюда.
    fn connect_extensions(self: &Rc<Self>) {
        {
            let weak = Rc::downgrade(self);
            self.ext_row.connect_activated(move |row| {
                let weak = weak.clone();
                extensions::present(row, move |info| {
                    if let Some(form) = weak.upgrade() {
                        form.extensions_changed(info.map(|info| info.id.as_str()));
                    }
                });
            });
        }
        {
            let weak = Rc::downgrade(self);
            self.ext_install.connect_clicked(move |button| {
                let Some(form) = weak.upgrade() else { return };
                let Some(offer) = form.offer.borrow().clone() else {
                    return;
                };
                button.set_sensitive(false);
                form.ext_row
                    .set_subtitle(&tr!("Installing {}…", offer.file_name()));
                let weak = weak.clone();
                let button = button.clone();
                extensions::install(extensions::Source::File(offer.path), move |result| {
                    button.set_sensitive(true);
                    let Some(form) = weak.upgrade() else { return };
                    match result {
                        Ok(info) => {
                            form.extensions_changed(Some(&info.id));
                            form.refresh_extensions_row(Some(&tr!(
                                "Installed: {} — selected in the database type list",
                                info.name
                            )));
                        }
                        Err(error) => {
                            form.refresh_extensions_row(None);
                            dialogs::message(
                                &form.widget,
                                tr!("Could Not Install Extension"),
                                &error,
                            );
                        }
                    }
                });
            });
        }
        {
            let weak = Rc::downgrade(self);
            *self.downloads_watch.borrow_mut() = extensions::watch_downloads(move || {
                if let Some(form) = weak.upgrade() {
                    form.refresh_extensions_row(None);
                }
            });
        }
        self.refresh_extensions_row(None);
    }

    /// `note` — что только что произошло (показывается первым).
    fn refresh_extensions_row(&self, note: Option<&str>) {
        let offer = db::extension::download_offers().into_iter().next();
        match &offer {
            Some(offer) => {
                self.ext_row.set_title(tr!("New extension downloaded"));
                let file = format!(
                    "{} — {}",
                    offer.file_name(),
                    extensions::age(offer.modified)
                );
                self.ext_row.set_subtitle(&match note {
                    Some(note) => tr!("{}. Also downloaded: {}", note, file),
                    None => file,
                });
            }
            None => {
                let count = db::extension::installed().len();
                self.ext_row.set_title(tr!("Other databases — extensions"));
                self.ext_row.set_subtitle(&if let Some(note) = note {
                    note.to_string()
                } else if count == 0 {
                    tr!("Download an extension — sshdeck will find it and offer to install it")
                        .to_string()
                } else {
                    tr!(
                        "Extensions installed: {}. To add more, just download the file",
                        count
                    )
                });
            }
        }
        self.ext_install.set_visible(offer.is_some());
        *self.offer.borrow_mut() = offer;
    }

    /// Расширение поставили или удалили: список СУБД пересобирается, новое
    /// расширение сразу выбирается.
    fn extensions_changed(&self, select: Option<&str>) {
        let current = self.selected();
        let (kind, driver) = self.start_kind.clone();
        let choices = kind_choices(&DbProfile::template_for(kind, driver));
        let labels: Vec<&str> = choices.iter().map(|choice| choice.label.as_str()).collect();
        let model = gtk::StringList::new(&labels);
        let position = choices
            .iter()
            .position(|choice| match select {
                Some(id) => choice.kind == DbKind::Extension && choice.driver == id,
                None => choice.kind == current.kind && choice.driver == current.driver,
            })
            .unwrap_or(0) as u32;
        *self.choices.borrow_mut() = choices;
        self.kind.set_model(Some(&model));
        self.kind.set_selected(position);
        // При замене модели выбор меняется без сигнала.
        self.kind_switched();
        self.refresh_extensions_row(None);
        if let Some(notify) = self.on_extensions.borrow().as_ref() {
            notify();
        }
    }

    fn sync_visibility(&self) {
        let networked = self.selected().template().is_networked();
        self.host.set_visible(networked);
        self.port.set_visible(networked);
        self.user.set_visible(networked);
        self.password.set_visible(networked);
        self.database.set_visible(networked);
        self.ssh.set_visible(networked);
        self.tls.set_visible(networked);
        self.path.set_visible(!networked);
    }

    /// Подставляет сохранённый пароль, если пользователь ещё не начал
    /// вводить свой.
    fn set_known_password(&self, password: Option<String>) {
        if !self.password_edited.get() {
            self.filling_password.set(true);
            self.password.set_text(password.as_deref().unwrap_or(""));
            self.filling_password.set(false);
        }
        *self.known_password.borrow_mut() = Some(password);
    }

    fn password_changed(&self) -> bool {
        self.password_edited.get()
    }

    /// Пароль, который нужно сохранить; `None` — сохранённый пароль ещё не
    /// загружен, а поле не трогали.
    fn password_to_save(&self) -> Option<String> {
        if self.password_edited.get() {
            return Some(self.password.text().to_string());
        }
        self.known_password
            .borrow()
            .clone()
            .map(|known| known.unwrap_or_default())
    }

    /// Порт из поля; `None` — в поле не число от 1 до 65535.
    fn port(&self, template: &DbProfile) -> Option<u16> {
        let text = self.port.text();
        match text.trim() {
            "" => Some(template.default_port()),
            text => text.parse::<u16>().ok().filter(|port| *port > 0),
        }
    }

    /// Профиль из полей; `None`, если чего-то не хватает.
    fn profile(&self) -> Option<DbProfile> {
        let choice = self.selected();
        let template = choice.template();
        let port = match self.port(&template) {
            Some(port) => port,
            None if template.is_networked() => return None,
            None => 0,
        };
        let profile = DbProfile {
            name: self.name.text().to_string(),
            kind: choice.kind,
            driver: choice.driver,
            host: self.host.text().to_string(),
            port,
            user: self.user.text().to_string(),
            database: self.database.text().to_string(),
            path: self.path.text().to_string(),
            ssh: self.ssh.text().to_string(),
            tls: self.tls.is_active(),
        }
        .normalized();
        profile.problem().is_none().then_some(profile)
    }

    /// Подсвечивает поля с ошибкой; причина — во всплывающей подсказке.
    fn mark_errors(&self) {
        let template = self.selected().template();
        let mark = |row: &adw::EntryRow, error: Option<String>| match error {
            Some(message) => {
                row.add_css_class("error");
                row.set_tooltip_text(Some(&message));
            }
            None => {
                row.remove_css_class("error");
                row.set_tooltip_text(None);
            }
        };
        mark(
            &self.port,
            (template.is_networked() && self.port(&template).is_none())
                .then(|| tr!("The port must be a number from 1 to 65535").to_string()),
        );
        let ssh = self.ssh.text();
        mark(
            &self.ssh,
            if ssh.trim().is_empty() {
                None
            } else {
                crate::session::validate_target(&ssh).err()
            },
        );
    }

    /// `on_change(valid)` вызывается при каждом изменении полей и сразу при
    /// подключении.
    fn connect_changed(self: &Rc<Self>, on_change: impl Fn(bool) + 'static) {
        let on_change = Rc::new(on_change);
        let notify = {
            let weak = Rc::downgrade(self);
            move || {
                if let Some(form) = weak.upgrade() {
                    form.mark_errors();
                    on_change(form.profile().is_some());
                }
            }
        };
        for row in [
            &self.name,
            &self.host,
            &self.port,
            &self.user,
            &self.database,
            &self.ssh,
            &self.path,
        ] {
            let notify = notify.clone();
            row.connect_changed(move |_| notify());
        }
        {
            let notify = notify.clone();
            self.kind.connect_selected_notify(move |_| notify());
        }
        notify();
    }
}
