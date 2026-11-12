//! Вкладка сессии: панель файлов слева, терминал справа, внизу — строка
//! состояния сервера.

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use vte::prelude::*;

use super::files::FilePanel;
use super::forwards::{self, SaveForwards, TabForwards};
use super::statusbar::StatusBar;
use super::terminal::{self, TerminalView};
use super::{ANIMATION_MS, Notifier, dialogs};
use crate::connections::Connection;
use crate::keysetup::{self, Installed, KeyPlan, LoginCheck};
use crate::remote::{self, CwdTracking};
use crate::session::Session;

/// Код, с которым ssh завершается при собственных ошибках (нет связи,
/// отказ в доступе). Остальные коды — это статус удалённой команды.
const SSH_ERROR_CODE: i32 = 255;

const INITIAL_PANEL_WIDTH: i32 = 340;

pub struct SessionTab {
    /// Корневой виджет вкладки; поверх него всплывают её уведомления.
    notifier: Notifier,
    term: vte::Terminal,
    panel: FilePanel,
    /// Панель файлов | терминал.
    paned: gtk::Paned,
    /// Ширина панели файлов до того, как её спрятали.
    panel_width: Cell<i32>,
    /// Идущая анимация показа или скрытия панели файлов.
    panel_animation: RefCell<Option<adw::TimedAnimation>>,
    status: StatusBar,
    session: Session,
    /// Проброс портов через мастер этой вкладки.
    forwards: Rc<TabForwards>,
    /// Полоса «соединение разорвано» с кнопкой переподключения.
    exit_banner: adw::Banner,
    /// Команда, отправляемая в терминал после входа (может быть пустой).
    startup: String,
    /// Отправлена ли она в текущем запуске ssh.
    startup_sent: Cell<bool>,
    /// Процесс ssh в терминале, пока он работает.
    ssh_pid: Cell<Option<glib::Pid>>,
    /// Имя сервера вкладки: `uname -n` через мастер, а до ответа — хост из
    /// первого сообщения OSC 7. Сообщения о каталоге с других хостов (после
    /// перехода с сервера дальше по ssh) панель файлов не трогают.
    server_host: RefCell<Option<String>>,
    /// Подсказка «терминал на другом сервере» уже показана.
    away_hint_shown: Cell<bool>,
    /// Последний каталог, о котором сообщил шелл этого запуска ssh. Первое
    /// приглашение обычно приходит раньше, чем панель подключится, — тогда
    /// панель переходит туда при подключении.
    last_cwd: RefCell<Option<PathBuf>>,
}

impl SessionTab {
    /// `startup` — команда после входа; `saved` — сохранённое подключение,
    /// из которого открыта вкладка (его пробросы портов ставятся после
    /// входа); `on_finished` вызывается, когда ssh завершился штатно и
    /// вкладку нужно закрыть.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target: &str,
        tab_id: u32,
        runtime_dir: &Path,
        font: &str,
        show_files: bool,
        startup: &str,
        saved: Option<Connection>,
        on_finished: impl Fn() + 'static,
    ) -> Rc<Self> {
        // Вкладки с тем же адресом делят одно мастер-соединение: вторая
        // вкладка к серверу пароль не спрашивает.
        let session = Session::shared(target, runtime_dir, crate::session::config_override());
        let scratch_dir = runtime_dir.join(format!("files-{}-{tab_id}", std::process::id()));
        let notifier = Notifier::new();
        let panel = FilePanel::new(&session, scratch_dir, notifier.clone());
        panel.widget().set_visible(show_files);
        let TerminalView { term, widget } = {
            let notifier = notifier.clone();
            TerminalView::new(font, move |text| notifier.info(text))
        };
        let status = StatusBar::new();
        let forwards = TabForwards::new(&session, saved, notifier.clone());

        let exit_banner = adw::Banner::builder()
            .button_label(tr!("Reconnect"))
            .revealed(false)
            .build();

        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(panel.widget())
            .end_child(&widget)
            .position(INITIAL_PANEL_WIDTH)
            .resize_start_child(false)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .vexpand(true)
            .css_classes(["sshdeck-split"])
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&exit_banner);
        content.append(&paned);
        content.append(status.widget());
        notifier.set_child(&content);

        let tab = Rc::new(Self {
            notifier,
            term,
            panel,
            paned,
            panel_width: Cell::new(INITIAL_PANEL_WIDTH),
            panel_animation: RefCell::new(None),
            status,
            session,
            forwards,
            exit_banner,
            startup: startup.trim().to_string(),
            startup_sent: Cell::new(false),
            ssh_pid: Cell::new(None),
            server_host: RefCell::new(None),
            away_hint_shown: Cell::new(false),
            last_cwd: RefCell::new(None),
        });

        {
            let weak = Rc::downgrade(&tab);
            let on_finished: Rc<dyn Fn()> = Rc::new(on_finished);
            tab.term.connect_child_exited(move |_, status| {
                let Some(tab) = weak.upgrade() else { return };
                tab.ssh_pid.set(None);
                match terminal::exit_code(status) {
                    Some(SSH_ERROR_CODE) => tab.show_exit(tr!(
                        "Could not connect, or the connection was lost — see the terminal for details"
                    )),
                    Some(_) => {
                        // Вкладку нельзя разрушать прямо из обработчика
                        // сигнала её же терминала.
                        let on_finished = Rc::clone(&on_finished);
                        glib::idle_add_local_once(move || on_finished());
                    }
                    None => tab.show_exit(tr!("ssh was terminated by a signal")),
                }
            });
        }
        {
            let weak = Rc::downgrade(&tab);
            tab.exit_banner.connect_button_clicked(move |_| {
                if let Some(tab) = weak.upgrade() {
                    tab.exit_banner.set_revealed(false);
                    tab.term.feed(b"\r\n");
                    tab.start_ssh();
                    tab.term.grab_focus();
                }
            });
        }
        {
            // Вход выполнен: можно опрашивать сервер и отправить команду.
            let weak = Rc::downgrade(&tab);
            tab.panel.set_connected_handler(move || {
                if let Some(tab) = weak.upgrade() {
                    tab.on_connected();
                }
            });
        }
        {
            let weak = Rc::downgrade(&tab);
            tab.panel.set_terminal_cd_handler(move |dir| {
                if let Some(tab) = weak.upgrade() {
                    // Пробел в начале не даёт команде попасть в историю
                    // оболочки (там, где это настроено).
                    tab.run_command(&format!(" cd {}", remote::shell_quote(dir)));
                }
            });
        }
        {
            // Оболочка на сервере сообщает текущий каталог (OSC 7) — панель
            // файлов переходит следом.
            let weak = Rc::downgrade(&tab);
            tab.term.connect_current_directory_uri_changed(move |term| {
                if let (Some(tab), Some(uri)) = (weak.upgrade(), term.current_directory_uri()) {
                    tab.on_cwd_reported(&uri);
                }
            });
        }

        tab.start_ssh();
        tab
    }

    pub fn widget(&self) -> &adw::ToastOverlay {
        self.notifier.widget()
    }

    pub fn target(&self) -> &str {
        self.session.target()
    }

    /// Сокет мастер-соединения вкладки (через него идут SSH-туннели к
    /// базам данных). Мастер может быть общим с другими вкладками того же
    /// адреса — см. описание модуля `session`.
    pub fn control_path(&self) -> &Path {
        self.session.control_path()
    }

    /// Пробросы портов вкладки.
    pub fn forwards(&self) -> &Rc<TabForwards> {
        &self.forwards
    }

    /// Способ записать пробросы в сохранённое подключение вкладки.
    pub fn set_forwards_save_handler(&self, handler: SaveForwards) {
        self.forwards.set_save_handler(handler);
    }

    /// Диалог «Проброс портов» для этой вкладки.
    pub fn show_forwards(&self) {
        forwards::show_dialog(self.widget(), &self.forwards);
    }

    /// Плавно показывает или прячет панель файлов: она выезжает слева,
    /// терминал занимает освободившееся место.
    pub fn set_files_visible(&self, visible: bool) {
        // Прошлая анимация доигрывается мгновенно, чтобы начать с конечного
        // состояния.
        if let Some(previous) = self.panel_animation.take() {
            previous.skip();
        }
        let panel = self.panel.widget().clone();
        if panel.is_visible() == visible {
            return;
        }
        let paned = self.paned.clone();
        if !visible && paned.position() > 0 {
            self.panel_width.set(paned.position());
        }
        let width = f64::from(self.panel_width.get().max(1));

        // На время анимации панель можно сжимать меньше её минимума.
        paned.set_shrink_start_child(true);
        if visible {
            panel.set_opacity(0.0);
            paned.set_position(0);
            panel.set_visible(true);
        }
        let (from, to) = if visible { (0.0, width) } else { (width, 0.0) };
        let target = adw::CallbackAnimationTarget::new({
            let paned = paned.clone();
            let panel = panel.clone();
            move |value| {
                paned.set_position(value.round() as i32);
                panel.set_opacity((value / width).clamp(0.0, 1.0));
            }
        });
        let animation = adw::TimedAnimation::builder()
            .widget(&paned)
            .value_from(from)
            .value_to(to)
            .duration(ANIMATION_MS + 40)
            .easing(adw::Easing::EaseOutCubic)
            .target(&target)
            .build();
        animation.connect_done(move |_| {
            panel.set_visible(visible);
            panel.set_opacity(1.0);
            paned.set_position(width as i32);
            paned.set_shrink_start_child(false);
        });
        animation.play();
        *self.panel_animation.borrow_mut() = Some(animation);
    }

    pub fn set_font(&self, font: &str) {
        terminal::apply_font(&self.term, font);
    }

    /// Перекрашивает терминал под выбранную тему.
    pub fn refresh_theme(&self) {
        terminal::refresh_colors(&self.term);
    }

    pub fn focus_terminal(&self) {
        self.term.grab_focus();
    }

    /// Отправляет команду в терминал так, как если бы её набрали и нажали
    /// Enter.
    pub fn run_command(&self, command: &str) {
        self.term.feed_child(format!("{command}\n").as_bytes());
        self.term.grab_focus();
    }

    /// Останавливает SFTP и опрос сервера, снимает пробросы портов, которые
    /// больше никому не нужны, убирает временные файлы и гасит
    /// мастер-соединение.
    ///
    /// Мастер общий для вкладок с тем же адресом, поэтому ssh этой вкладки
    /// завершается отдельно (SIGHUP), а сам мастер гасится, только если
    /// вкладка была последней, кто им пользовался.
    pub fn shutdown(&self) {
        self.status.stop();
        self.panel.shutdown();
        self.forwards.shutdown();
        if let Some(pid) = self.ssh_pid.take() {
            terminal::hang_up(pid);
        }
        self.session.close_master();
    }

    fn on_connected(self: &Rc<Self>) {
        self.status.start(&self.session);
        self.query_server_host();
        let last_cwd = self.last_cwd.borrow().clone();
        if let Some(path) = last_cwd {
            self.panel.follow_dir(&path);
        }
        if !self.startup.is_empty() && !self.startup_sent.replace(true) {
            self.term
                .feed_child(format!("{}\n", self.startup).as_bytes());
        }
    }

    /// Узнаёт имя сервера через мастер, чтобы отличать сообщения OSC 7 от
    /// него и от серверов, на которые перешли из его терминала.
    fn query_server_host(self: &Rc<Self>) {
        let session = self.session.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || remote::server_hostname(&session)).await;
            if let (Some(tab), Ok(Ok(name))) = (weak.upgrade(), result) {
                *tab.server_host.borrow_mut() = Some(name);
            }
        });
    }

    /// Оболочка сообщила текущий каталог (OSC 7). Панель файлов переходит
    /// туда, если сообщение пришло с сервера этой вкладки.
    fn on_cwd_reported(&self, uri: &str) {
        let Some(dir) = remote::parse_cwd_uri(uri) else { return };
        let server = self.server_host.borrow().clone();
        match server {
            Some(server) if !remote::same_host(&server, &dir.host) => {
                // Терминал на другом сервере: путь относится к нему.
                if !self.away_hint_shown.replace(true) {
                    self.notifier.info(&tr!(
                        "The terminal is now on {}; the file panel stays on {}",
                        dir.host,
                        server
                    ));
                }
                return;
            }
            Some(_) => {}
            None => {
                // Пустой хост и localhost о сервере ничего не говорят.
                if !dir.host.is_empty() && !dir.host.eq_ignore_ascii_case("localhost") {
                    *self.server_host.borrow_mut() = Some(dir.host.clone());
                }
            }
        }
        self.away_hint_shown.set(false);
        // Путь — сырые байты: имя каталога не обязано быть в UTF-8.
        let path = Path::new(OsStr::from_bytes(&dir.path)).to_path_buf();
        self.panel.follow_dir(&path);
        *self.last_cwd.borrow_mut() = Some(path);
    }

    /// Вход без пароля: определяет ключ, который ssh использует для этого
    /// хоста (`ssh -G`), и после подтверждения создаёт его (если его нет) и
    /// ставит открытую часть на сервер этой вкладки.
    pub fn setup_key_login(self: &Rc<Self>) {
        let target = self.session.target().to_string();
        let config = self.session.config().map(Path::to_path_buf);
        let home = glib::home_dir();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result =
                gio::spawn_blocking(move || keysetup::plan(&target, config.as_deref(), &home))
                    .await;
            let Some(tab) = weak.upgrade() else { return };
            match result {
                Ok(Ok(plan)) => tab.confirm_key_login(plan),
                Ok(Err(message)) => tab.notifier.error(&message),
                Err(_) => tab
                    .notifier
                    .error(tr!("Key login setup was interrupted by an internal error")),
            }
        });
    }

    fn confirm_key_login(self: &Rc<Self>, plan: KeyPlan) {
        let target = self.session.target().to_string();
        let key = short_path(&plan.private);
        // Рядом с ключом может не быть .pub — тогда показываем сам ключ.
        let existing = if plan.public().is_file() {
            short_path(&plan.public())
        } else {
            key.clone()
        };
        let key_note = match (plan.create, plan.explicit) {
            (false, false) => tr!("Your key {} will be used.", existing),
            (false, true) => tr!(
                "Your key {} will be used: it is set by IdentityFile for this host.",
                existing
            ),
            (true, false) => tr!(
                "There is no local key yet: {} will be created (ed25519, no passphrase).",
                key
            ),
            (true, true) => tr!(
                "The key {} set by IdentityFile for this host does not exist yet: it will be \
                 created (ed25519, no passphrase).",
                key
            ),
        };
        let body = tr!(
            "{}\n\nIts public part will be added to ~/.ssh/authorized_keys on {}. After that, \
             ssh will log in to this server with the key instead of asking for a password. \
             The app never sees or stores the password itself.",
            key_note,
            target
        );
        let weak = Rc::downgrade(self);
        dialogs::confirm(
            self.widget(),
            tr!("Set Up Passwordless Login?"),
            &body,
            tr!("Set Up"),
            false,
            move || {
                let Some(tab) = weak.upgrade() else { return };
                let session = tab.session.clone();
                let weak = Rc::downgrade(&tab);
                glib::spawn_future_local(async move {
                    // ssh и ssh-keygen блокируют: работаем вне потока GTK.
                    let result =
                        gio::spawn_blocking(move || keysetup::setup(&session, &plan)).await;
                    let Some(tab) = weak.upgrade() else { return };
                    match result {
                        Ok(Ok(outcome)) => tab.show_key_outcome(&outcome, &target),
                        Ok(Err(message)) => tab.notifier.error(&message),
                        Err(_) => tab
                            .notifier
                            .error(tr!("Key login setup was interrupted by an internal error")),
                    }
                });
            },
        );
    }

    fn show_key_outcome(&self, outcome: &keysetup::Outcome, target: &str) {
        let key = short_path(&outcome.public_key);
        let created = if outcome.created {
            tr!("A new key {} was created.", key) + "\n\n"
        } else {
            String::new()
        };
        let (heading, body) = match (&outcome.login, outcome.installed) {
            // Ключ на месте, но сервер его не принял: скорее всего, sshd
            // читает ключи из другого файла.
            (LoginCheck::Fails(detail), _) => (
                tr!("Key Installed, but Login Still Fails"),
                created
                    + &tr!(
                        "Key {} is in ~/.ssh/authorized_keys on {}, but a test login with it \
                         still failed ({}). The server’s sshd may read keys from a different \
                         file (AuthorizedKeysFile in /etc/ssh/sshd_config) or ignore ~/.ssh \
                         because its permissions are too open.",
                        key,
                        target,
                        detail
                    ),
            ),
            (_, Installed::Added) => (
                tr!("Passwordless Login Enabled"),
                created
                    + &tr!(
                        "Key {} is installed on {}. Next time you will log in \
                         without a password.",
                        key,
                        target
                    ),
            ),
            (_, Installed::AlreadyPresent) => (
                tr!("Key Already Installed"),
                created
                    + &tr!(
                        "Key {} is already on {}; nothing had to be changed.",
                        key,
                        target
                    ),
            ),
        };
        dialogs::message(self.widget(), heading, &body);
    }

    /// Слежение панели файлов за `cd`: оболочка на сервере должна сообщать
    /// терминалу текущий каталог. fish делает это сам, для bash и zsh в
    /// файл настроек оболочки дописывается короткий фрагмент.
    pub fn setup_cwd_tracking(self: &Rc<Self>) {
        let target = self.session.target().to_string();
        let weak = Rc::downgrade(self);
        dialogs::confirm(
            self.widget(),
            tr!("Follow cd in the File Panel?"),
            &tr!(
                "For the file panel to follow cd in the terminal, the shell on {} must report \
                 the current folder. fish does this by itself. For bash and zsh, three lines \
                 marked sshdeck-cwd will be appended to ~/.bashrc or ~/.zshrc on the server; \
                 they take effect in new sessions.",
                target
            ),
            tr!("Enable"),
            false,
            move || {
                let Some(tab) = weak.upgrade() else { return };
                let session = tab.session.clone();
                let weak = Rc::downgrade(&tab);
                glib::spawn_future_local(async move {
                    let result =
                        gio::spawn_blocking(move || remote::install_cwd_tracking(&session)).await;
                    let Some(tab) = weak.upgrade() else { return };
                    let (heading, body) = match result {
                        Ok(Ok(CwdTracking::Added)) => (
                            tr!("Following cd Enabled"),
                            tr!(
                                "The snippet was added to the shell settings file. It takes effect \
                                 after you log in again: close the tab and reconnect."
                            ),
                        ),
                        Ok(Ok(CwdTracking::Updated)) => (
                            tr!("Following cd Updated"),
                            tr!(
                                "The old sshdeck-cwd snippet was replaced with a new version that \
                                 also handles folders with spaces and non-ASCII names. It takes \
                                 effect after you log in again: close the tab and reconnect."
                            ),
                        ),
                        Ok(Ok(CwdTracking::Present)) => (
                            tr!("Following cd Already Enabled"),
                            tr!("The sshdeck-cwd snippet is already in the shell settings file."),
                        ),
                        Ok(Ok(CwdTracking::Builtin)) => (
                            tr!("Nothing to Change"),
                            tr!("The server uses fish, which reports the current folder by itself."),
                        ),
                        Ok(Ok(CwdTracking::Unsupported)) => (
                            tr!("Shell Not Supported"),
                            tr!(
                                "Only bash and zsh can be set up automatically. No files on the \
                                 server were changed."
                            ),
                        ),
                        Ok(Err(message)) => {
                            tab.notifier.error(&message);
                            return;
                        }
                        Err(_) => {
                            tab.notifier
                                .error(tr!("Setup was interrupted by an internal error"));
                            return;
                        }
                    };
                    dialogs::message(tab.widget(), heading, body);
                });
            },
        );
    }

    fn start_ssh(self: &Rc<Self>) {
        self.panel.set_ssh_running(true);
        self.startup_sent.set(false);
        self.last_cwd.borrow_mut().take();
        let weak = Rc::downgrade(self);
        terminal::spawn(&self.term, &self.session.terminal_argv(), move |result| {
            let Some(tab) = weak.upgrade() else { return };
            match result {
                Ok(pid) => tab.ssh_pid.set(Some(pid)),
                Err(error) => tab.show_exit(&tr!("Could not start ssh: {}", error)),
            }
        });
    }

    fn show_exit(&self, message: &str) {
        self.status.stop();
        self.panel.set_ssh_running(false);
        self.exit_banner.set_title(message);
        self.exit_banner.set_revealed(true);
    }
}

/// Путь для показа пользователю: домашний каталог сокращается до `~`.
fn short_path(path: &Path) -> String {
    match path.strip_prefix(glib::home_dir()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}
