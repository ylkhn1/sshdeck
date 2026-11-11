//! Вкладка сессии: панель файлов слева, терминал справа, внизу — строка
//! состояния сервера.

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use vte::prelude::*;

use super::files::FilePanel;
use super::statusbar::StatusBar;
use super::terminal::{self, TerminalView};
use super::{Notifier, dialogs};
use crate::keysetup::{self, Installed};
use crate::remote::{self, CwdTracking};
use crate::session::Session;

/// Код, с которым ssh завершается при собственных ошибках (нет связи,
/// отказ в доступе). Остальные коды — это статус удалённой команды.
const SSH_ERROR_CODE: i32 = 255;

const INITIAL_PANEL_WIDTH: i32 = 420;

pub struct SessionTab {
    /// Корневой виджет вкладки; поверх него всплывают её уведомления.
    notifier: Notifier,
    term: vte::Terminal,
    panel: FilePanel,
    status: StatusBar,
    session: Session,
    /// Полоса «соединение разорвано» с кнопкой переподключения.
    exit_banner: adw::Banner,
    /// Команда, отправляемая в терминал после входа (может быть пустой).
    startup: String,
    /// Отправлена ли она в текущем запуске ssh.
    startup_sent: Cell<bool>,
}

impl SessionTab {
    /// `startup` — команда после входа; `on_finished` вызывается, когда ssh
    /// завершился штатно и вкладку нужно закрыть.
    pub fn new(
        target: &str,
        tab_id: u32,
        runtime_dir: &Path,
        font: &str,
        startup: &str,
        on_finished: impl Fn() + 'static,
    ) -> Rc<Self> {
        let session = Session::new(
            target,
            runtime_dir,
            tab_id,
            crate::session::config_override(),
        );
        let scratch_dir = runtime_dir.join(format!("files-{}-{tab_id}", std::process::id()));
        let notifier = Notifier::new();
        let panel = FilePanel::new(&session, scratch_dir, notifier.clone());
        let TerminalView { term, widget } = {
            let notifier = notifier.clone();
            TerminalView::new(font, move |text| notifier.info(text))
        };
        let status = StatusBar::new();

        let exit_banner = adw::Banner::builder()
            .button_label("Переподключиться")
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
            status,
            session,
            exit_banner,
            startup: startup.trim().to_string(),
            startup_sent: Cell::new(false),
        });

        {
            let weak = Rc::downgrade(&tab);
            let on_finished: Rc<dyn Fn()> = Rc::new(on_finished);
            tab.term.connect_child_exited(move |_, status| {
                let Some(tab) = weak.upgrade() else { return };
                match terminal::exit_code(status) {
                    Some(SSH_ERROR_CODE) => tab.show_exit(
                        "Соединение не установлено или разорвано — подробности в терминале",
                    ),
                    Some(_) => {
                        // Вкладку нельзя разрушать прямо из обработчика
                        // сигнала её же терминала.
                        let on_finished = Rc::clone(&on_finished);
                        glib::idle_add_local_once(move || on_finished());
                    }
                    None => tab.show_exit("ssh был прерван сигналом"),
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
                let path = term
                    .current_directory_uri()
                    .and_then(|uri| glib::filename_from_uri(&uri).ok())
                    .and_then(|(path, _host)| path.to_str().map(str::to_string));
                if let (Some(tab), Some(path)) = (weak.upgrade(), path) {
                    tab.panel.follow_dir(&path);
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

    /// Останавливает SFTP и опрос сервера, убирает временные файлы и гасит
    /// мастер-соединение.
    pub fn shutdown(&self) {
        self.status.stop();
        self.panel.shutdown();
        self.session.close_master();
    }

    fn on_connected(&self) {
        self.status.start(&self.session);
        if !self.startup.is_empty() && !self.startup_sent.replace(true) {
            self.term
                .feed_child(format!("{}\n", self.startup).as_bytes());
        }
    }

    /// Вход без пароля: после подтверждения создаёт локальный ключ (если
    /// его нет) и ставит открытую часть на сервер этой вкладки.
    pub fn setup_key_login(self: &Rc<Self>) {
        let ssh_dir = glib::home_dir().join(".ssh");
        let target = self.session.target().to_string();
        let key_note = match keysetup::existing_key(&ssh_dir) {
            Some(public) => format!("Будет использован ваш ключ {}.", short_path(&public)),
            None => format!(
                "Локального ключа пока нет: будет создан {} (ed25519, без парольной фразы).",
                short_path(&keysetup::new_key_path(&ssh_dir))
            ),
        };
        let body = format!(
            "{key_note}\n\nЕго открытая часть будет дописана в ~/.ssh/authorized_keys \
             на {target}. После этого ssh будет входить на этот сервер по ключу, \
             не спрашивая пароль. Сам пароль приложение не видит и не сохраняет."
        );
        let weak = Rc::downgrade(self);
        dialogs::confirm(
            self.widget(),
            "Настроить вход без пароля?",
            &body,
            "Настроить",
            false,
            move || {
                let Some(tab) = weak.upgrade() else { return };
                let session = tab.session.clone();
                let weak = Rc::downgrade(&tab);
                glib::spawn_future_local(async move {
                    // ssh и ssh-keygen блокируют: работаем вне потока GTK.
                    let result =
                        gio::spawn_blocking(move || keysetup::setup(&session, &ssh_dir)).await;
                    let Some(tab) = weak.upgrade() else { return };
                    match result {
                        Ok(Ok(outcome)) => {
                            let key = short_path(&outcome.public_key);
                            let created = if outcome.created {
                                format!("Создан новый ключ {key}.\n\n")
                            } else {
                                String::new()
                            };
                            let (heading, body) = match outcome.installed {
                                Installed::Added => (
                                    "Вход без пароля настроен",
                                    format!(
                                        "{created}Ключ {key} установлен на {target}. \
                                         Следующий вход пройдёт без пароля."
                                    ),
                                ),
                                Installed::AlreadyPresent => (
                                    "Ключ уже установлен",
                                    format!(
                                        "{created}Ключ {key} уже есть на {target}, \
                                         ничего менять не пришлось."
                                    ),
                                ),
                            };
                            dialogs::message(tab.widget(), heading, &body);
                        }
                        Ok(Err(message)) => tab.notifier.error(&message),
                        Err(_) => tab
                            .notifier
                            .error("Настройка входа по ключу прервалась из-за внутренней ошибки"),
                    }
                });
            },
        );
    }

    /// Слежение панели файлов за `cd`: оболочка на сервере должна сообщать
    /// терминалу текущий каталог. fish делает это сам, для bash и zsh в
    /// файл настроек оболочки дописывается короткий фрагмент.
    pub fn setup_cwd_tracking(self: &Rc<Self>) {
        let target = self.session.target().to_string();
        let weak = Rc::downgrade(self);
        dialogs::confirm(
            self.widget(),
            "Включить слежение за cd?",
            &format!(
                "Чтобы панель файлов переходила вслед за cd в терминале, оболочка на \
                 {target} должна сообщать текущий каталог. fish делает это сам. Для bash и \
                 zsh в конец ~/.bashrc или ~/.zshrc на сервере будет дописано три строки \
                 с пометкой sshdeck-cwd; они действуют в новых сеансах."
            ),
            "Включить",
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
                            "Слежение включено",
                            "Фрагмент дописан в файл настроек оболочки. Он заработает после \
                             нового входа: закройте вкладку и подключитесь снова.",
                        ),
                        Ok(Ok(CwdTracking::Present)) => (
                            "Слежение уже включено",
                            "Фрагмент sshdeck-cwd уже есть в файле настроек оболочки.",
                        ),
                        Ok(Ok(CwdTracking::Builtin)) => (
                            "Ничего менять не нужно",
                            "На сервере оболочка fish: она сама сообщает текущий каталог.",
                        ),
                        Ok(Ok(CwdTracking::Unsupported)) => (
                            "Оболочка не поддерживается",
                            "Автоматически настраиваются только bash и zsh. Файлы сервера \
                             не изменены.",
                        ),
                        Ok(Err(message)) => {
                            tab.notifier.error(&message);
                            return;
                        }
                        Err(_) => {
                            tab.notifier
                                .error("Настройка прервалась из-за внутренней ошибки");
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
        let weak = Rc::downgrade(self);
        terminal::spawn(&self.term, &self.session.terminal_argv(), move |error| {
            if let Some(tab) = weak.upgrade() {
                tab.show_exit(&format!("Не удалось запустить ssh: {error}"));
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
