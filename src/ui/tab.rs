//! Вкладка сессии: панель файлов слева, терминал справа, внизу — строка
//! состояния сервера. Терминал можно делить на несколько частей: все они
//! ходят на сервер через общее мастер-соединение вкладки.

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use vte::prelude::*;

use super::files::FilePanel;
use super::forwards::{self, SaveForwards, TabForwards};
use super::statusbar::StatusBar;
use super::terminal::{self, TerminalView};
use super::{ANIMATION_MS, Notifier, dialogs};
use crate::connections::Connection;
use crate::keysetup::{self, Installed, KeyPlan, LoginCheck};
use crate::remote::{self, CwdTracking};
use crate::session::{LoginTicket, LoginTurn, Session};

/// Код, с которым ssh завершается при собственных ошибках (нет связи,
/// отказ в доступе). Остальные коды — это статус удалённой команды.
const SSH_ERROR_CODE: i32 = 255;

const INITIAL_PANEL_WIDTH: i32 = 340;

/// Как часто терминал, ждущий чужого входа, проверяет мастер.
const LOGIN_POLL: Duration = Duration::from_millis(200);

/// Действие над терминалами вкладки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneAction {
    /// Новый терминал справа от текущего.
    SplitRight,
    /// Новый терминал под текущим.
    SplitDown,
    /// Закрыть текущий терминал (если он не единственный).
    Close,
}

/// Один терминал вкладки со своим процессом ssh.
struct Pane {
    term: vte::Terminal,
    /// Терминал с полосой прокрутки — именно он стоит в раскладке.
    widget: gtk::ScrolledWindow,
    /// Процесс ssh, пока он работает.
    pid: Cell<Option<glib::Pid>>,
    /// Место в очереди входа на мастер вкладки.
    login: LoginTicket,
    /// Опрос мастера, пока терминал ждёт входа в другом терминале.
    waiting: RefCell<Option<glib::SourceId>>,
}

impl Pane {
    /// Перестаёт ждать чужого входа (если ждал).
    fn stop_waiting(&self) {
        if let Some(source) = self.waiting.take() {
            source.remove();
        }
    }

    /// Терминал больше не входит и не ждёт: уходит из очереди входа.
    fn leave_login(&self) {
        self.stop_waiting();
        self.login.leave();
    }
}

pub struct SessionTab {
    /// Корневой виджет вкладки; поверх него всплывают её уведомления.
    notifier: Notifier,
    /// Терминалы вкладки в порядке создания; пустым список не бывает.
    panes: RefCell<Vec<Rc<Pane>>>,
    /// Терминал, в котором сейчас работают: в него идут команды, за его
    /// каталогом следует панель файлов.
    active: RefCell<Option<Rc<Pane>>>,
    /// Корень раскладки терминалов: один терминал или дерево `gtk::Paned`.
    terms: gtk::Box,
    /// Шрифт терминалов — для тех, что появятся при делении.
    font: RefCell<String>,
    /// Вызывается, когда последний терминал вкладки завершился штатно.
    on_finished: Rc<dyn Fn()>,
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
    /// Панель файлов подключилась в текущем запуске ssh.
    connected: Cell<bool>,
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
        show_local_files: bool,
        startup: &str,
        saved: Option<Connection>,
        on_finished: impl Fn() + 'static,
    ) -> Rc<Self> {
        // Вкладки с тем же адресом делят одно мастер-соединение: вторая
        // вкладка к серверу пароль не спрашивает.
        let session = Session::shared(target, runtime_dir, crate::session::config_override());
        let scratch_dir = runtime_dir.join(format!("files-{}-{tab_id}", std::process::id()));
        let notifier = Notifier::new();
        let panel = FilePanel::new(&session, scratch_dir, notifier.clone(), show_local_files);
        panel.widget().set_visible(show_files);
        let terms = gtk::Box::builder()
            .hexpand(true)
            .vexpand(true)
            .css_classes(["sshdeck-terms"])
            .build();
        let status = StatusBar::new();
        let forwards = TabForwards::new(&session, saved, notifier.clone());

        let exit_banner = adw::Banner::builder()
            .button_label(tr!("Reconnect"))
            .revealed(false)
            .build();

        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(panel.widget())
            .end_child(&terms)
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
            panes: RefCell::new(Vec::new()),
            active: RefCell::new(None),
            terms,
            font: RefCell::new(font.to_string()),
            on_finished: Rc::new(on_finished),
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
            connected: Cell::new(false),
            server_host: RefCell::new(None),
            away_hint_shown: Cell::new(false),
            last_cwd: RefCell::new(None),
        });

        let first = tab.add_pane();
        tab.terms.append(&first.widget);
        tab.set_active(&first);
        {
            let weak = Rc::downgrade(&tab);
            tab.exit_banner.connect_button_clicked(move |_| {
                if let Some(tab) = weak.upgrade() {
                    // Полоса появляется, только когда терминал остался один.
                    let pane = tab.active_pane();
                    tab.exit_banner.set_revealed(false);
                    pane.term.feed(b"\r\n");
                    tab.start_ssh(&pane);
                    pane.term.grab_focus();
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
                    tab.run_command(&remote::cd_command(dir));
                }
            });
        }
        tab.start_ssh(&first);
        tab
    }

    /// Создаёт терминал вкладки (ещё не вставленный в раскладку) и
    /// подключает его сигналы.
    fn add_pane(self: &Rc<Self>) -> Rc<Pane> {
        let TerminalView { term, widget } = {
            let notifier = self.notifier.clone();
            TerminalView::new(&self.font.borrow(), move |text| notifier.info(text))
        };
        widget.add_css_class("sshdeck-pane");
        let pane = Rc::new(Pane {
            term,
            widget,
            pid: Cell::new(None),
            login: self.session.login_ticket(),
            waiting: RefCell::new(None),
        });

        // Терминал держит обработчики, поэтому в них — только слабые ссылки.
        let link = || (Rc::downgrade(self), Rc::downgrade(&pane));
        {
            let (weak_tab, weak_pane) = link();
            pane.term.connect_child_exited(move |_, status| {
                if let (Some(tab), Some(pane)) = (weak_tab.upgrade(), weak_pane.upgrade()) {
                    tab.on_pane_exited(&pane, status);
                }
            });
        }
        {
            // Оболочка на сервере сообщает текущий каталог (OSC 7) — панель
            // файлов переходит следом, если это терминал, где сейчас работают.
            let (weak_tab, weak_pane) = link();
            pane.term.connect_current_directory_uri_changed(move |term| {
                let (Some(tab), Some(pane)) = (weak_tab.upgrade(), weak_pane.upgrade()) else {
                    return;
                };
                if let (true, Some(uri)) = (tab.is_active(&pane), term.current_directory_uri()) {
                    tab.on_cwd_reported(&uri);
                }
            });
        }
        {
            let focus = gtk::EventControllerFocus::new();
            let (weak_tab, weak_pane) = link();
            focus.connect_enter(move |_| {
                if let (Some(tab), Some(pane)) = (weak_tab.upgrade(), weak_pane.upgrade()) {
                    tab.set_active(&pane);
                }
            });
            pane.term.add_controller(focus);
        }
        {
            // Как и копирование, сочетания ловятся до терминала: иначе
            // Ctrl+Shift+W и прочие ушли бы на сервер.
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            let (weak_tab, weak_pane) = link();
            keys.connect_key_pressed(move |_, keyval, keycode, state| {
                let (Some(tab), Some(pane)) = (weak_tab.upgrade(), weak_pane.upgrade()) else {
                    return glib::Propagation::Proceed;
                };
                if pane.waiting.borrow().is_some()
                    && matches!(keyval, gdk::Key::Return | gdk::Key::KP_Enter)
                {
                    // Не ждать чужого входа, а войти в этом терминале.
                    pane.leave_login();
                    tab.update_login_note();
                    pane.term.feed(b"\r\n");
                    tab.launch_ssh(&pane);
                    return glib::Propagation::Stop;
                }
                match pane_shortcut(&pane.term, keyval, keycode, state) {
                    Some(action) => {
                        tab.set_active(&pane);
                        tab.pane_action(action);
                        glib::Propagation::Stop
                    }
                    None => glib::Propagation::Proceed,
                }
            });
            pane.term.add_controller(keys);
        }

        self.panes.borrow_mut().push(Rc::clone(&pane));
        pane
    }

    /// Терминал, в котором сейчас работают.
    fn active_pane(&self) -> Rc<Pane> {
        let active = self.active.borrow().clone();
        active.unwrap_or_else(|| Rc::clone(&self.panes.borrow()[0]))
    }

    fn is_active(&self, pane: &Rc<Pane>) -> bool {
        self.active
            .borrow()
            .as_ref()
            .is_some_and(|active| Rc::ptr_eq(active, pane))
    }

    fn has_pane(&self, pane: &Rc<Pane>) -> bool {
        self.panes.borrow().iter().any(|p| Rc::ptr_eq(p, pane))
    }

    /// Делает терминал текущим. Панель файлов переходит в его каталог,
    /// если он уже известен.
    fn set_active(&self, pane: &Rc<Pane>) {
        let previous = self.active.replace(Some(Rc::clone(pane)));
        if let Some(previous) = previous {
            if Rc::ptr_eq(&previous, pane) {
                return;
            }
            previous.widget.remove_css_class("active");
        }
        pane.widget.add_css_class("active");
        if let Some(uri) = pane.term.current_directory_uri() {
            self.on_cwd_reported(&uri);
        }
    }

    /// Деление или закрытие текущего терминала (меню и сочетания клавиш).
    pub fn pane_action(self: &Rc<Self>, action: PaneAction) {
        match action {
            PaneAction::SplitRight => self.split(gtk::Orientation::Horizontal),
            PaneAction::SplitDown => self.split(gtk::Orientation::Vertical),
            PaneAction::Close => {
                let pane = self.active_pane();
                self.close_pane(&pane);
            }
        }
    }

    /// Делит текущий терминал пополам; в новой половине открывается ещё
    /// одна сессия через то же мастер-соединение.
    fn split(self: &Rc<Self>, orientation: gtk::Orientation) {
        if self.exit_banner.is_revealed() {
            self.notifier
                .info(tr!("Reconnect first: the tab is not connected"));
            return;
        }
        let current: gtk::Widget = self.active_pane().widget.clone().upcast();
        let size = match orientation {
            gtk::Orientation::Horizontal => current.width(),
            _ => current.height(),
        };
        let pane = self.add_pane();
        let split = gtk::Paned::builder()
            .orientation(orientation)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .css_classes(["sshdeck-split"])
            .build();
        replace_in_layout(&self.terms, &current, split.upcast_ref());
        split.set_start_child(Some(&current));
        split.set_end_child(Some(&pane.widget));
        if size > 0 {
            split.set_position(size / 2);
        }
        self.terms.add_css_class("split");
        self.set_active(&pane);
        pane.term.grab_focus();
        self.spawn_ssh(&pane);
    }

    /// Закрывает терминал, если он не единственный: его ssh получает
    /// SIGHUP, место занимает соседний терминал.
    fn close_pane(&self, pane: &Rc<Pane>) {
        if self.panes.borrow().len() <= 1 {
            self.notifier
                .info(tr!("This is the only terminal in the tab"));
            return;
        }
        self.panes.borrow_mut().retain(|p| !Rc::ptr_eq(p, pane));
        pane.leave_login();
        if let Some(pid) = pane.pid.take() {
            terminal::hang_up(pid);
        }
        self.remove_from_layout(pane);
        self.update_login_note();
    }

    /// Убирает терминал из раскладки: его место занимает соседняя половина.
    /// Из списка терминалов он к этому моменту уже удалён.
    fn remove_from_layout(&self, pane: &Rc<Pane>) {
        let widget: gtk::Widget = pane.widget.clone().upcast();
        let Some(parent) = widget.parent().and_downcast::<gtk::Paned>() else {
            return;
        };
        let sibling = if parent.start_child().as_ref() == Some(&widget) {
            parent.end_child()
        } else {
            parent.start_child()
        };
        let Some(sibling) = sibling else { return };
        // Текущим становится терминал, занимающий освободившееся место.
        // Фокус переводится до перестройки: иначе GtkPaned теряет свой
        // фокусный дочерний виджет и пишет предупреждение.
        let next = if self.is_active(pane) {
            let panes = self.panes.borrow();
            panes
                .iter()
                .find(|p| {
                    p.widget.is_ancestor(&sibling)
                        || p.widget.upcast_ref::<gtk::Widget>() == &sibling
                })
                .or(panes.first())
                .cloned()
        } else {
            None
        };
        if let Some(next) = &next {
            next.term.grab_focus();
        }
        // На время перестройки фокус снимается совсем, потом возвращается.
        let root = self.terms.root();
        let focus = root.as_ref().and_then(RootExt::focus);
        if let Some(root) = &root {
            RootExt::set_focus(root, gtk::Widget::NONE);
        }
        parent.set_start_child(gtk::Widget::NONE);
        parent.set_end_child(gtk::Widget::NONE);
        replace_in_layout(&self.terms, parent.upcast_ref(), &sibling);
        if let Some(focus) = focus.filter(|w| w.root().is_some()) {
            focus.grab_focus();
        }
        if self.panes.borrow().len() == 1 {
            self.terms.remove_css_class("split");
        }
        if let Some(next) = next {
            self.set_active(&next);
            next.term.grab_focus();
        }
    }

    /// ssh в одном из терминалов завершился.
    fn on_pane_exited(self: &Rc<Self>, pane: &Rc<Pane>, status: i32) {
        pane.pid.set(None);
        // Если этот ssh входил на сервер и не вошёл, входить начинает
        // следующий ждущий терминал.
        pane.login.leave();
        // Терминал, закрытый пользователем, уже убран.
        if !self.has_pane(pane) {
            return;
        }
        let code = terminal::exit_code(status);
        if self.panes.borrow().len() > 1 {
            // Не последний терминал вкладки: просто закрываем его.
            if code == Some(SSH_ERROR_CODE) {
                self.notifier
                    .info(tr!("A split terminal lost its connection and was closed"));
            }
            self.panes.borrow_mut().retain(|p| !Rc::ptr_eq(p, pane));
            // Терминал нельзя убирать прямо из обработчика его же сигнала.
            let weak = Rc::downgrade(self);
            let pane = Rc::clone(pane);
            glib::idle_add_local_once(move || {
                if let Some(tab) = weak.upgrade() {
                    tab.remove_from_layout(&pane);
                }
            });
            return;
        }
        match code {
            Some(SSH_ERROR_CODE) => self.show_exit(tr!(
                "Could not connect, or the connection was lost — see the terminal for details"
            )),
            Some(_) => {
                // Вкладку нельзя разрушать прямо из обработчика сигнала её
                // же терминала.
                let on_finished = Rc::clone(&self.on_finished);
                glib::idle_add_local_once(move || on_finished());
            }
            None => self.show_exit(tr!("ssh was terminated by a signal")),
        }
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

    /// Показывает или прячет дерево этого компьютера в панели файлов (без
    /// вызова обработчика переключения).
    pub fn set_local_files_visible(&self, visible: bool) {
        self.panel.set_local_files_visible(visible);
    }

    /// Пользователь включил или выключил дерево этого компьютера.
    pub fn set_local_files_handler(&self, handler: impl Fn(bool) + 'static) {
        self.panel.set_local_toggle_handler(handler);
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
        *self.font.borrow_mut() = font.to_string();
        for pane in self.panes.borrow().iter() {
            terminal::apply_font(&pane.term, font);
        }
    }

    /// Перекрашивает терминалы под выбранную тему.
    pub fn refresh_theme(&self) {
        for pane in self.panes.borrow().iter() {
            terminal::refresh_colors(&pane.term);
        }
    }

    pub fn focus_terminal(&self) {
        self.active_pane().term.grab_focus();
    }

    /// Отправляет команду в текущий терминал так, как если бы её набрали и
    /// нажали Enter.
    pub fn run_command(&self, command: &str) {
        let pane = self.active_pane();
        pane.term.feed_child(format!("{command}\n").as_bytes());
        pane.term.grab_focus();
    }

    /// Останавливает SFTP и опрос сервера, снимает пробросы портов, которые
    /// больше никому не нужны, убирает временные файлы и гасит
    /// мастер-соединение.
    ///
    /// Мастер общий для вкладок с тем же адресом, поэтому ssh терминалов
    /// этой вкладки завершаются отдельно (SIGHUP), а сам мастер гасится,
    /// только если вкладка была последней, кто им пользовался.
    pub fn shutdown(&self) {
        self.status.stop();
        self.panel.shutdown();
        self.forwards.shutdown();
        for pane in self.panes.borrow().iter() {
            pane.leave_login();
            if let Some(pid) = pane.pid.take() {
                terminal::hang_up(pid);
            }
        }
        self.session.close_master();
    }

    fn on_connected(self: &Rc<Self>) {
        self.connected.set(true);
        self.status.start(&self.session);
        self.query_server_host();
        let last_cwd = self.last_cwd.borrow().clone();
        if let Some(path) = last_cwd {
            self.panel.follow_dir(&path);
        }
        self.send_startup();
    }

    /// Отправляет команду после входа, когда панель файлов подключилась, а
    /// ssh текущего терминала запущен: терминал, ждавший чужого входа,
    /// может запустить ssh уже после подключения панели.
    fn send_startup(&self) {
        if self.startup.is_empty() || !self.connected.get() {
            return;
        }
        let pane = self.active_pane();
        if pane.pid.get().is_some() && !self.startup_sent.replace(true) {
            pane.term
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

    /// Первый запуск ssh вкладки или переподключение: состояние сессии
    /// начинается заново.
    fn start_ssh(self: &Rc<Self>, pane: &Rc<Pane>) {
        self.panel.set_ssh_running(true);
        self.startup_sent.set(false);
        self.connected.set(false);
        self.last_cwd.borrow_mut().take();
        self.spawn_ssh(pane);
    }

    /// Запускает ssh в терминале. Если вход уже выполнен, ssh подключается
    /// через мастер и пароль не спрашивает. Если на этот сервер прямо
    /// сейчас входят в другом терминале, ssh запускается, только когда
    /// тот вход закончится (см. `session::LoginTicket`).
    fn spawn_ssh(self: &Rc<Self>, pane: &Rc<Pane>) {
        pane.stop_waiting();
        match pane.login.turn() {
            LoginTurn::MasterUp | LoginTurn::LogIn => self.launch_ssh(pane),
            LoginTurn::Wait => self.wait_for_login(pane),
        }
        self.update_login_note();
    }

    /// Если все терминалы вкладки ждут чужого входа, панель файлов говорит,
    /// что вход идёт в другой вкладке, а не просит ввести пароль здесь.
    fn update_login_note(&self) {
        let elsewhere = {
            let panes = self.panes.borrow();
            !panes.is_empty() && panes.iter().all(|p| p.waiting.borrow().is_some())
        };
        self.panel.set_login_elsewhere(elsewhere);
    }

    /// Ждёт, пока закончится вход в другом терминале: опрашивает мастер и
    /// запускает ssh, когда тот поднялся или когда очередь входить дошла до
    /// этого терминала.
    fn wait_for_login(self: &Rc<Self>, pane: &Rc<Pane>) {
        let note = format!(
            "\x1b[2m{}\r\n{}\x1b[0m\r\n",
            tr!("Waiting for login in another terminal…"),
            tr!("Press Enter to log in here instead.")
        );
        pane.term.feed(note.as_bytes());
        let weak_tab = Rc::downgrade(self);
        let weak_pane = Rc::downgrade(pane);
        let source = glib::timeout_add_local(LOGIN_POLL, move || {
            let (Some(tab), Some(pane)) = (weak_tab.upgrade(), weak_pane.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            let turn = pane.login.turn();
            if turn == LoginTurn::Wait {
                return glib::ControlFlow::Continue;
            }
            // Источник снимается возвратом Break, а не через remove().
            pane.waiting.take();
            tab.update_login_note();
            if turn == LoginTurn::LogIn {
                let note = format!(
                    "\x1b[2m{}\x1b[0m\r\n",
                    tr!("The other login did not complete — logging in here.")
                );
                pane.term.feed(note.as_bytes());
            }
            tab.launch_ssh(&pane);
            glib::ControlFlow::Break
        });
        *pane.waiting.borrow_mut() = Some(source);
    }

    /// Запускает процесс ssh в терминале.
    fn launch_ssh(self: &Rc<Self>, pane: &Rc<Pane>) {
        let weak_tab = Rc::downgrade(self);
        let weak_pane = Rc::downgrade(pane);
        terminal::spawn(&pane.term, &self.session.terminal_argv(), move |result| {
            let (Some(tab), Some(pane)) = (weak_tab.upgrade(), weak_pane.upgrade()) else {
                return;
            };
            match result {
                Ok(pid) => {
                    // Терминал могли закрыть, пока ssh запускался.
                    if tab.has_pane(&pane) {
                        pane.pid.set(Some(pid));
                        tab.send_startup();
                    } else {
                        terminal::hang_up(pid);
                    }
                }
                Err(error) => {
                    pane.login.leave();
                    let message = tr!("Could not start ssh: {}", error);
                    if tab.panes.borrow().len() > 1 {
                        tab.notifier.error(&message);
                        tab.close_pane(&pane);
                    } else {
                        tab.show_exit(&message);
                    }
                }
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

/// Ставит `new` в раскладке терминалов на место `old`; `old` остаётся без
/// родителя. `new` не должен быть ни в каком контейнере.
fn replace_in_layout(root: &gtk::Box, old: &gtk::Widget, new: &gtk::Widget) {
    let Some(parent) = old.parent() else { return };
    match parent.downcast_ref::<gtk::Paned>() {
        Some(paned) if paned.start_child().as_ref() == Some(old) => {
            paned.set_start_child(Some(new));
        }
        Some(paned) => paned.set_end_child(Some(new)),
        None => {
            root.remove(old);
            root.append(new);
        }
    }
}

/// Сочетание клавиш для терминалов вкладки, как в Terminator:
/// Ctrl+Shift+E — разделить вправо, Ctrl+Shift+O — вниз, Ctrl+Shift+W —
/// закрыть текущий. Буква определяется по физической клавише, поэтому
/// сочетания работают и в русской раскладке.
fn pane_shortcut(
    term: &vte::Terminal,
    keyval: gdk::Key,
    keycode: u32,
    state: gdk::ModifierType,
) -> Option<PaneAction> {
    let mods = state
        & (gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SUPER_MASK);
    if mods != gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK {
        return None;
    }
    match terminal::latin_letter(term, keyval, keycode)? {
        'e' => Some(PaneAction::SplitRight),
        'o' => Some(PaneAction::SplitDown),
        'w' => Some(PaneAction::Close),
        _ => None,
    }
}

/// Путь для показа пользователю: домашний каталог сокращается до `~`.
fn short_path(path: &Path) -> String {
    match path.strip_prefix(glib::home_dir()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}
