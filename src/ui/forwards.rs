//! Проброс портов вкладки: состояние пробросов, их постановка в мастер и
//! диалог «Проброс портов».
//!
//! Логика (счётчик вкладок, состояния, смена мастера) — в
//! `crate::forward`; здесь — опрос мастера, очередь команд ssh и виджеты.
//! Вкладка раз в [`POLL`] спрашивает `ssh -O check`, пока у неё есть
//! пробросы: так видно и первый вход, и переподключение (у нового мастера
//! другой pid), после которого пробросы ставятся снова.
//!
//! Команды одному мастеру выполняются строго по очереди (одна очередь на
//! сокет для всех вкладок): иначе отмена проброса, убранного в одной
//! вкладке, могла бы обогнать его постановку в другой.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use super::Notifier;
use crate::connections::Connection;
use crate::forward::{self, Entry, Forward, Kind, State, Tracker};
use crate::session::Session;

/// Как часто проверять мастер, пока у вкладки есть пробросы.
const POLL: Duration = Duration::from_secs(1);

/// Меняет пробросы сохранённого подключения: находит подключение (каким
/// оно было после прошлой записи), применяет правку к его списку пробросов,
/// сохраняет файл и возвращает подключение в новом виде.
pub type SaveForwards = Rc<dyn Fn(&Connection, &dyn Fn(&mut Vec<Forward>)) -> Result<Connection, String>>;

type Listener = Rc<dyn Fn(&TabForwards)>;

/// Пробросы одной вкладки.
pub struct TabForwards {
    me: Weak<Self>,
    session: Session,
    tracker: RefCell<Tracker>,
    /// Сохранённое подключение, из которого открыта вкладка, в том виде, в
    /// каком оно записано сейчас.
    saved: RefCell<Option<Connection>>,
    save: RefCell<Option<SaveForwards>>,
    listeners: RefCell<Vec<(u32, Listener)>>,
    next_listener: Cell<u32>,
    polling: Cell<bool>,
    closed: Cell<bool>,
    notifier: Notifier,
}

impl TabForwards {
    /// Пробросы сохранённого подключения `saved` ставятся сами, как только
    /// вкладка войдёт на сервер.
    pub fn new(session: &Session, saved: Option<Connection>, notifier: Notifier) -> Rc<Self> {
        let mut tracker = Tracker::new(session.control_path());
        if let Some(connection) = &saved {
            for forward in &connection.forwards {
                // Список в файле уже проверен; повторов и конфликтов в нём
                // нет (см. Connection::with_forwards).
                let _ = tracker.add(forward.clone(), true);
            }
        }
        let this = Rc::new_cyclic(|me| Self {
            me: me.clone(),
            session: session.clone(),
            tracker: RefCell::new(tracker),
            saved: RefCell::new(saved),
            save: RefCell::new(None),
            listeners: RefCell::new(Vec::new()),
            next_listener: Cell::new(1),
            polling: Cell::new(false),
            closed: Cell::new(false),
            notifier,
        });
        this.ensure_polling();
        this
    }

    pub fn target(&self) -> &str {
        self.session.target()
    }

    /// Название сохранённого подключения вкладки.
    pub fn saved_name(&self) -> Option<String> {
        self.saved.borrow().as_ref().map(|c| c.name.clone())
    }

    /// Способ записать пробросы в сохранённое подключение.
    pub fn set_save_handler(&self, handler: SaveForwards) {
        *self.save.borrow_mut() = Some(handler);
    }

    /// Вызывается после каждого изменения списка или состояний.
    pub fn connect_changed(&self, listener: impl Fn(&TabForwards) + 'static) -> u32 {
        let id = self.next_listener.get();
        self.next_listener.set(id + 1);
        self.listeners.borrow_mut().push((id, Rc::new(listener)));
        id
    }

    pub fn disconnect(&self, id: u32) {
        self.listeners.borrow_mut().retain(|(other, _)| *other != id);
    }

    fn changed(&self) {
        let listeners: Vec<Listener> = self.listeners.borrow().iter().map(|(_, l)| Rc::clone(l)).collect();
        for listener in listeners {
            listener(self);
        }
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.tracker.borrow().entries().to_vec()
    }

    pub fn has_master(&self) -> bool {
        self.tracker.borrow().has_master()
    }

    /// Добавляет проброс; с `save` — и в сохранённое подключение.
    pub fn add(&self, forward: Forward, save: bool) -> Result<(), String> {
        let save = save && self.saved.borrow().is_some();
        let jobs = self.tracker.borrow_mut().add(forward.clone(), save)?;
        self.run(jobs);
        if save {
            self.persist(&|list: &mut Vec<Forward>| {
                if !list.contains(&forward) {
                    list.push(forward.clone());
                }
            });
        }
        self.ensure_polling();
        self.changed();
        Ok(())
    }

    /// Убирает проброс из вкладки (и из сохранённого подключения, если он
    /// там записан). В мастере он снимается, только если больше ни одна
    /// вкладка его не просила.
    pub fn remove(&self, forward: &Forward) {
        let saved = self
            .tracker
            .borrow()
            .entries()
            .iter()
            .any(|entry| &entry.forward == forward && entry.saved);
        let Some(last) = self.tracker.borrow_mut().remove(forward) else {
            return;
        };
        if last {
            self.enqueue_cancel(forward.clone());
        }
        if saved {
            self.persist(&|list: &mut Vec<Forward>| list.retain(|other| other != forward));
        }
        self.changed();
    }

    /// Ещё одна попытка для проброса, который не удалось поставить.
    pub fn retry(&self, forward: &Forward) {
        let jobs = self.tracker.borrow_mut().retry(forward);
        self.run(jobs);
        self.changed();
    }

    /// Записывает проброс в сохранённое подключение или убирает его оттуда.
    pub fn set_saved(&self, forward: &Forward, saved: bool) {
        self.tracker.borrow_mut().set_saved(forward, saved);
        if saved {
            self.persist(&|list: &mut Vec<Forward>| {
                if !list.contains(forward) {
                    list.push(forward.clone());
                }
            });
        } else {
            self.persist(&|list: &mut Vec<Forward>| list.retain(|other| other != forward));
        }
        self.changed();
    }

    /// Вкладка закрывается: пробросы, которые больше никому не нужны,
    /// снимаются в мастере.
    pub fn shutdown(&self) {
        self.closed.set(true);
        let released = self.tracker.borrow_mut().release_all();
        for forward in released {
            self.enqueue_cancel(forward);
        }
        self.listeners.borrow_mut().clear();
    }

    fn persist(&self, edit: &dyn Fn(&mut Vec<Forward>)) {
        let handler = self.save.borrow().clone();
        let original = self.saved.borrow().clone();
        let (Some(handler), Some(original)) = (handler, original) else {
            return;
        };
        match handler(&original, edit) {
            Ok(updated) => *self.saved.borrow_mut() = Some(updated),
            Err(message) => self.notifier.error(&message),
        }
    }

    fn run(&self, jobs: Vec<forward::Job>) {
        for (forward, generation) in jobs {
            enqueue(Queued {
                session: self.session.clone(),
                forward,
                op: Op::Apply {
                    generation,
                    owner: self.me.clone(),
                },
            });
        }
    }

    fn enqueue_cancel(&self, forward: Forward) {
        enqueue(Queued {
            session: self.session.clone(),
            forward,
            op: Op::Cancel,
        });
    }

    /// Запускает опрос мастера, если он ещё не идёт и есть что ставить.
    fn ensure_polling(&self) {
        if self.polling.get() || self.closed.get() || self.tracker.borrow().entries().is_empty() {
            return;
        }
        self.polling.set(true);
        let weak = self.me.clone();
        glib::spawn_future_local(async move {
            loop {
                let session = {
                    let Some(this) = weak.upgrade() else { return };
                    if this.closed.get() || this.tracker.borrow().entries().is_empty() {
                        this.polling.set(false);
                        return;
                    }
                    this.session.clone()
                };
                // ssh -O check блокирует: работаем вне потока GTK.
                let pid = gio::spawn_blocking(move || forward::master_pid(&session))
                    .await
                    .ok()
                    .flatten();
                {
                    let Some(this) = weak.upgrade() else { return };
                    if this.closed.get() {
                        return;
                    }
                    let (jobs, master_changed) = {
                        let mut tracker = this.tracker.borrow_mut();
                        let generation = tracker.generation();
                        let jobs = tracker.master(pid);
                        (jobs, tracker.generation() != generation)
                    };
                    let changed = master_changed || !jobs.is_empty();
                    this.run(jobs);
                    if changed {
                        this.changed();
                    }
                }
                glib::timeout_future(POLL).await;
            }
        });
    }
}

// ------------------------------------------------------- очередь команд

enum Op {
    /// Поставить проброс для поколения мастера `generation` вкладки `owner`.
    Apply {
        generation: u64,
        owner: Weak<TabForwards>,
    },
    /// Снять проброс, если его так никто и не попросил снова.
    Cancel,
}

struct Queued {
    session: Session,
    forward: Forward,
    op: Op,
}

thread_local! {
    /// Очереди команд по сокетам мастеров. Очередь есть, пока она
    /// выполняется.
    static QUEUES: RefCell<HashMap<PathBuf, VecDeque<Queued>>> = RefCell::new(HashMap::new());
}

fn enqueue(job: Queued) {
    let path = job.session.control_path().to_path_buf();
    let start = QUEUES.with_borrow_mut(|queues| {
        let start = !queues.contains_key(&path);
        queues.entry(path.clone()).or_default().push_back(job);
        start
    });
    if start {
        glib::spawn_future_local(drain(path));
    }
}

async fn drain(path: PathBuf) {
    loop {
        let job = QUEUES.with_borrow_mut(|queues| {
            let queue = queues.get_mut(&path)?;
            let job = queue.pop_front();
            if job.is_none() {
                queues.remove(&path);
            }
            job
        });
        let Some(job) = job else { break };
        run_job(job).await;
    }
}

async fn run_job(job: Queued) {
    let Queued { session, forward, op } = job;
    match op {
        Op::Apply { generation, owner } => {
            let wanted = owner.upgrade().is_some_and(|owner| {
                !owner.closed.get() && owner.tracker.borrow().is_applying(&forward, generation)
            });
            if !wanted {
                return;
            }
            let (s, f) = (session.clone(), forward.clone());
            let result = gio::spawn_blocking(move || forward::apply(&s, &f))
                .await
                .unwrap_or_else(|_| Err(tr!("The forward was interrupted by an internal error").to_string()));
            match owner.upgrade() {
                Some(owner) if !owner.closed.get() => {
                    let accepted = owner.tracker.borrow_mut().finish(&forward, generation, result);
                    if accepted {
                        owner.changed();
                    }
                }
                // Вкладку закрыли, пока шла команда: проброс, который больше
                // никому не нужен, снимается.
                _ if result.is_ok() && forward::users(session.control_path(), &forward) == 0 => {
                    let _ = gio::spawn_blocking(move || forward::cancel(&session, &forward)).await;
                }
                _ => {}
            }
        }
        Op::Cancel => {
            // Пока команда ждала очереди, проброс могла попросить другая
            // вкладка.
            if forward::users(session.control_path(), &forward) > 0 {
                return;
            }
            let _ = gio::spawn_blocking(move || forward::cancel(&session, &forward)).await;
        }
    }
}

// ------------------------------------------------------- значок вкладки

/// Значок на ярлыке вкладки и подсказка к нему: есть работающие или
/// неудачные пробросы. `None` — значок не нужен.
pub fn indicator(forwards: &TabForwards) -> Option<(&'static str, String)> {
    let entries = forwards.entries();
    let failed = entries.iter().any(|entry| matches!(entry.state, State::Failed(_)));
    let active = entries.iter().any(|entry| entry.state == State::Active);
    let icon = if failed {
        "dialog-warning-symbolic"
    } else if active {
        "network-transmit-receive-symbolic"
    } else {
        return None;
    };
    let mut tooltip = tr!("Port forwarding").to_string();
    for entry in &entries {
        tooltip.push_str(&format!(
            "\n{} — {}",
            entry.forward.summary(),
            state_text(&entry.state, forwards.has_master())
        ));
    }
    Some((icon, tooltip))
}

fn state_text(state: &State, has_master: bool) -> String {
    match state {
        State::Waiting if has_master => tr!("Waiting…").to_string(),
        State::Waiting => tr!("Waiting for login").to_string(),
        State::Applying => tr!("Setting up…").to_string(),
        State::Active => tr!("Active").to_string(),
        State::Failed(message) => tr!("Failed: {}", message),
    }
}

// ------------------------------------------------------------ диалог

/// Чем отличаются поля нового проброса для каждого вида.
fn kind_texts(kind: Kind) -> (&'static str, &'static str, &'static str) {
    match kind {
        Kind::Local => (
            tr!("A port on this computer leads to a host and port reachable from the server."),
            tr!("Port on this computer"),
            tr!("Host as seen from the server"),
        ),
        Kind::Remote => (
            tr!("A port on the server leads to a host and port reachable from this computer."),
            tr!("Port on the server"),
            tr!("Host as seen from this computer"),
        ),
        Kind::Dynamic => (
            tr!(
                "A SOCKS proxy on this computer: programs that use it reach the network \
                 through the server."
            ),
            tr!("SOCKS port on this computer"),
            "",
        ),
    }
}

fn flat_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build()
}

/// Диалог «Проброс портов» вкладки: список пробросов с состоянием и форма
/// нового проброса.
pub fn show_dialog(parent: &impl IsA<gtk::Widget>, forwards: &Rc<TabForwards>) {
    let page = adw::PreferencesPage::new();
    let list_group = adw::PreferencesGroup::builder()
        .title(tr!("Forwards"))
        .description(tr!(
            "Forwards go through this tab’s SSH connection and are shared with other tabs \
             to {}. They stop when the connection closes and are set up again after you \
             reconnect.",
            forwards.target()
        ))
        .build();
    page.add(&list_group);

    let add_group = adw::PreferencesGroup::builder().title(tr!("New Forward")).build();
    let labels: Vec<&str> = Kind::ALL.iter().map(|kind| kind.label()).collect();
    let kind_row = adw::ComboRow::builder()
        .title(tr!("Type"))
        .model(&gtk::StringList::new(&labels))
        .build();
    let entry = |title: &str| adw::EntryRow::builder().title(title).build();
    let bind_row = entry(tr!("Listen address (optional)"));
    bind_row.set_tooltip_text(Some(tr!(
        "Empty: only connections from the same machine (localhost). * means all \
         interfaces. Write IPv6 addresses in square brackets, for example [::1]."
    )));
    let port_row = entry("");
    port_row.set_input_purpose(gtk::InputPurpose::Digits);
    let host_row = entry("");
    host_row.set_text("localhost");
    let host_port_row = entry(tr!("Destination port"));
    host_port_row.set_input_purpose(gtk::InputPurpose::Digits);
    add_group.add(&kind_row);
    add_group.add(&bind_row);
    add_group.add(&port_row);
    add_group.add(&host_row);
    add_group.add(&host_port_row);
    let save_row = adw::SwitchRow::builder().title(tr!("Save with Connection")).build();
    match forwards.saved_name() {
        Some(name) => {
            save_row.set_subtitle(&tr!("Set up automatically whenever “{}” connects", name));
            add_group.add(&save_row);
        }
        None => save_row.set_visible(false),
    }
    let error = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .margin_top(6)
        .visible(false)
        .css_classes(["error", "caption"])
        .build();
    add_group.add(&error);
    let add_button = gtk::Button::builder()
        .label(tr!("Add Forward"))
        .halign(gtk::Align::Center)
        .margin_top(12)
        .sensitive(false)
        .css_classes(["suggested-action", "pill"])
        .build();
    add_group.add(&add_button);
    page.add(&add_group);

    let toasts = adw::ToastOverlay::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&page));
    toasts.set_child(Some(&view));
    let dialog = adw::Dialog::builder()
        .title(tr!("Port Forwarding"))
        .content_width(580)
        .content_height(640)
        .child(&toasts)
        .build();

    // --- форма
    let kind_of = {
        let kind_row = kind_row.downgrade();
        move || {
            let index = kind_row.upgrade().map_or(0, |row| row.selected()) as usize;
            Kind::ALL.get(index).copied().unwrap_or(Kind::Local)
        }
    };
    let read_form: Rc<dyn Fn() -> Result<Forward, String>> = {
        let (bind, port, host, host_port) = (
            bind_row.downgrade(),
            port_row.downgrade(),
            host_row.downgrade(),
            host_port_row.downgrade(),
        );
        let kind_of = kind_of.clone();
        Rc::new(move || {
            let text = |row: &glib::WeakRef<adw::EntryRow>| {
                row.upgrade().map(|row| row.text().to_string()).unwrap_or_default()
            };
            Forward::from_fields(kind_of(), &text(&bind), &text(&port), &text(&host), &text(&host_port))
        })
    };
    // Ошибка видна, когда пользователь начал вводить порт.
    let validate: Rc<dyn Fn()> = {
        let read_form = Rc::clone(&read_form);
        let (port, error, add_button) = (port_row.downgrade(), error.downgrade(), add_button.downgrade());
        Rc::new(move || {
            let (Some(port), Some(error), Some(add_button)) = (port.upgrade(), error.upgrade(), add_button.upgrade())
            else {
                return;
            };
            let result = read_form();
            add_button.set_sensitive(result.is_ok());
            match result {
                Err(message) if !port.text().trim().is_empty() => {
                    error.set_label(&message);
                    error.set_visible(true);
                }
                _ => error.set_visible(false),
            }
        })
    };
    let sync_kind = {
        let (add_group, port, host, host_port) = (
            add_group.downgrade(),
            port_row.downgrade(),
            host_row.downgrade(),
            host_port_row.downgrade(),
        );
        let validate = Rc::clone(&validate);
        move |kind: Kind| {
            let (description, port_title, host_title) = kind_texts(kind);
            if let (Some(group), Some(port), Some(host), Some(host_port)) =
                (add_group.upgrade(), port.upgrade(), host.upgrade(), host_port.upgrade())
            {
                group.set_description(Some(description));
                port.set_title(port_title);
                host.set_title(host_title);
                host.set_visible(kind != Kind::Dynamic);
                host_port.set_visible(kind != Kind::Dynamic);
            }
            validate();
        }
    };
    sync_kind(Kind::Local);
    {
        let kind_of = kind_of.clone();
        kind_row.connect_selected_notify(move |_| sync_kind(kind_of()));
    }
    for row in [&bind_row, &port_row, &host_row, &host_port_row] {
        let validate = Rc::clone(&validate);
        row.connect_changed(move |_| validate());
    }
    let submit: Rc<dyn Fn()> = {
        let weak = Rc::downgrade(forwards);
        let read_form = Rc::clone(&read_form);
        let (port, host_port, save, error) = (
            port_row.downgrade(),
            host_port_row.downgrade(),
            save_row.downgrade(),
            error.downgrade(),
        );
        Rc::new(move || {
            let Some(forwards) = weak.upgrade() else { return };
            let save = save.upgrade().is_some_and(|row| row.is_active());
            let result = read_form().and_then(|forward| forwards.add(forward, save));
            match result {
                Ok(()) => {
                    for row in [&port, &host_port] {
                        if let Some(row) = row.upgrade() {
                            row.set_text("");
                        }
                    }
                    if let Some(port) = port.upgrade() {
                        port.grab_focus();
                    }
                }
                Err(message) => {
                    if let Some(error) = error.upgrade() {
                        error.set_label(&message);
                        error.set_visible(true);
                    }
                }
            }
        })
    };
    {
        let submit = Rc::clone(&submit);
        add_button.connect_clicked(move |_| submit());
    }
    for row in [&bind_row, &port_row, &host_row, &host_port_row] {
        let submit = Rc::clone(&submit);
        let add_button = add_button.downgrade();
        row.connect_entry_activated(move |_| {
            if add_button.upgrade().is_some_and(|button| button.is_sensitive()) {
                submit();
            }
        });
    }

    // --- список
    let rows: Rc<RefCell<Vec<gtk::Widget>>> = Rc::new(RefCell::new(Vec::new()));
    let refresh = {
        let (group, toasts) = (list_group.downgrade(), toasts.downgrade());
        let rows = Rc::clone(&rows);
        let owner = Rc::downgrade(forwards);
        move |forwards: &TabForwards| {
            let (Some(group), Some(toasts)) = (group.upgrade(), toasts.upgrade()) else { return };
            for row in rows.borrow_mut().drain(..) {
                group.remove(&row);
            }
            let entries = forwards.entries();
            if entries.is_empty() {
                let placeholder = adw::ActionRow::builder()
                    .title(tr!("No forwards yet"))
                    .subtitle(tr!(
                        "Add one below, for example to open a web app or a database \
                         from the server on this computer."
                    ))
                    .build();
                placeholder.add_css_class("dim-label");
                group.add(&placeholder);
                rows.borrow_mut().push(placeholder.upcast());
            }
            let saved_name = forwards.saved_name();
            for entry in entries {
                let row = entry_row(&entry, forwards.has_master(), saved_name.as_deref(), &owner, &toasts);
                group.add(&row);
                rows.borrow_mut().push(row.upcast());
            }
        }
    };
    refresh(forwards);
    let id = forwards.connect_changed(refresh);
    {
        let weak = Rc::downgrade(forwards);
        dialog.connect_closed(move |_| {
            if let Some(forwards) = weak.upgrade() {
                forwards.disconnect(id);
            }
        });
    }
    dialog.set_focus(Some(&port_row));
    dialog.present(Some(parent));
}

/// Строка проброса: что куда ведёт, состояние и действия.
fn entry_row(
    entry: &Entry,
    has_master: bool,
    saved_name: Option<&str>,
    owner: &Weak<TabForwards>,
    toasts: &adw::ToastOverlay,
) -> adw::ActionRow {
    let forward = &entry.forward;
    let row = adw::ActionRow::builder()
        .title(forward.summary())
        .subtitle(format!("{} · {}", forward.kind.label(), state_text(&entry.state, has_master)))
        .use_markup(false)
        .build();
    let status: gtk::Widget = match &entry.state {
        State::Active => gtk::Image::builder()
            .icon_name("object-select-symbolic")
            .css_classes(["success"])
            .build()
            .upcast(),
        State::Failed(_) => gtk::Image::builder()
            .icon_name("dialog-error-symbolic")
            .css_classes(["error"])
            .build()
            .upcast(),
        State::Waiting | State::Applying => adw::Spinner::new().upcast(),
    };
    row.add_prefix(&status);
    if let State::Failed(_) = entry.state {
        row.add_css_class("error");
    }

    if let Some(address) = forward.local_address() {
        let button = flat_button("edit-copy-symbolic", &tr!("Copy {}", address));
        let toasts = toasts.downgrade();
        button.connect_clicked(move |button| {
            button.clipboard().set_text(&address);
            if let Some(toasts) = toasts.upgrade() {
                toasts.add_toast(adw::Toast::new(&tr!("Copied: {}", address)));
            }
        });
        row.add_suffix(&button);
    }
    if let Some(url) = forward.browser_url() {
        let button = flat_button("web-browser-symbolic", tr!("Open in Browser"));
        let toasts = toasts.downgrade();
        button.connect_clicked(move |button| {
            let window = button.root().and_downcast::<gtk::Window>();
            let toasts = toasts.clone();
            gtk::UriLauncher::new(&url).launch(window.as_ref(), gio::Cancellable::NONE, move |result| {
                if let (Err(err), Some(toasts)) = (result, toasts.upgrade())
                    && !err.matches(gtk::DialogError::Dismissed)
                {
                    toasts.add_toast(adw::Toast::new(&tr!("Could not open the browser: {}", err)));
                }
            });
        });
        row.add_suffix(&button);
    }
    if let State::Failed(_) = entry.state {
        let button = flat_button("view-refresh-symbolic", tr!("Try Again"));
        let (owner, forward) = (owner.clone(), forward.clone());
        button.connect_clicked(move |_| {
            if let Some(forwards) = owner.upgrade() {
                forwards.retry(&forward);
            }
        });
        row.add_suffix(&button);
    }
    if let Some(name) = saved_name {
        let tooltip = if entry.saved {
            tr!("Saved with “{}”: set up automatically when it connects", name)
        } else {
            tr!("Save with “{}”", name)
        };
        let button = gtk::ToggleButton::builder()
            .icon_name(if entry.saved { "starred-symbolic" } else { "non-starred-symbolic" })
            .active(entry.saved)
            .tooltip_text(tooltip)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let (owner, forward) = (owner.clone(), forward.clone());
        button.connect_toggled(move |button| {
            let active = button.is_active();
            let owner = owner.clone();
            let forward = forward.clone();
            // Список перестраивается — не из обработчика самой кнопки.
            glib::idle_add_local_once(move || {
                if let Some(forwards) = owner.upgrade() {
                    forwards.set_saved(&forward, active);
                }
            });
        });
        row.add_suffix(&button);
    }
    let remove = flat_button("user-trash-symbolic", tr!("Remove"));
    let (owner, forward) = (owner.clone(), forward.clone());
    remove.connect_clicked(move |_| {
        let owner = owner.clone();
        let forward = forward.clone();
        glib::idle_add_local_once(move || {
            if let Some(forwards) = owner.upgrade() {
                forwards.remove(&forward);
            }
        });
    });
    row.add_suffix(&remove);
    row
}
