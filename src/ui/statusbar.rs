//! Строка состояния внизу вкладки: загрузка CPU, память, диск, сеть и
//! аптайм сервера.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use super::format_size;
use crate::remote::{self, Stream};
use crate::session::Session;
use crate::sysstat::{self, Sample, Usage};

/// Пороги, после которых значение подсвечивается.
const WARN_PERCENT: f64 = 75.0;
const CRITICAL_PERCENT: f64 = 90.0;

pub struct StatusBar {
    inner: Rc<Inner>,
}

struct Inner {
    root: gtk::ScrolledWindow,
    cpu: Gauge,
    memory: Gauge,
    disk: Gauge,
    network: gtk::Label,
    load: gtk::Label,
    uptime: gtk::Label,
    stream: RefCell<Option<Stream>>,
    previous: RefCell<Option<Sample>>,
}

impl StatusBar {
    pub fn new() -> Self {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.add_css_class("sshdeck-statusbar");
        // Ячейка «название — [полоска] — значение»; возвращает значение и
        // полоску, если она нужна.
        let cell = |title: &str, tooltip: &str, with_bar: bool| {
            let value = gtk::Label::builder()
                .label("—")
                .css_classes(["numeric"])
                .build();
            let cell = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            cell.add_css_class("sshdeck-stat");
            cell.set_tooltip_text(Some(tooltip));
            cell.append(
                &gtk::Label::builder()
                    .label(title)
                    .css_classes(["dim-label", "sshdeck-stat-title"])
                    .build(),
            );
            let bar = with_bar.then(|| {
                let bar = gtk::ProgressBar::builder()
                    .css_classes(["sshdeck-meter"])
                    .valign(gtk::Align::Center)
                    .build();
                cell.append(&bar);
                bar
            });
            cell.append(&value);
            row.append(&cell);
            (value, bar)
        };
        let gauge = |title: &str, tooltip: &str| {
            let (value, bar) = cell(title, tooltip, true);
            Gauge {
                value,
                bar: bar.expect("cell was asked for a bar"),
            }
        };
        let item = |title: &str, tooltip: &str| cell(title, tooltip, false).0;
        let cpu = gauge("CPU", tr!("Server CPU usage"));
        let memory = gauge(tr!("RAM"), tr!("Memory in use out of the server’s total memory"));
        let disk = gauge(tr!("Disk /"), tr!("Root partition usage"));
        let network = item(tr!("Network"), tr!("Receive and send rates across all server interfaces"));
        let uptime = item(tr!("Uptime"), tr!("Time since the server was last rebooted"));
        let load = item(trc!("load average", "Load"), tr!("One-minute load average"));

        // Прокрутка без полосы: строка не задаёт минимальную ширину окна.
        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .child(&row)
            .visible(false)
            .build();

        Self {
            inner: Rc::new(Inner {
                root,
                cpu,
                memory,
                disk,
                network,
                load,
                uptime,
                stream: RefCell::new(None),
                previous: RefCell::new(None),
            }),
        }
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.inner.root
    }

    /// Начинает опрос сервера через мастер-соединение сессии. Повторный
    /// вызов при уже идущем опросе ничего не делает.
    pub fn start(&self, session: &Session) {
        let inner = &self.inner;
        if inner
            .stream
            .borrow()
            .as_ref()
            .is_some_and(Stream::is_running)
        {
            return;
        }
        *inner.previous.borrow_mut() = None;

        let (tx, rx) = async_channel::unbounded::<Vec<String>>();
        let mut block: Vec<String> = Vec::new();
        let stream = remote::stream(session, &remote::sh(sysstat::SCRIPT), move |line| {
            match line.trim() {
                sysstat::BLOCK_BEGIN => block.clear(),
                sysstat::BLOCK_END => return tx.send_blocking(std::mem::take(&mut block)).is_ok(),
                _ => block.push(line),
            }
            true
        });
        // Без показателей вкладка остаётся рабочей, поэтому сбой запуска
        // просто оставляет строку скрытой.
        let Ok(stream) = stream else { return };
        *inner.stream.borrow_mut() = Some(stream);

        let weak = Rc::downgrade(inner);
        glib::spawn_future_local(async move {
            while let Ok(block) = rx.recv().await {
                let Some(inner) = weak.upgrade() else { return };
                inner.show(&block);
            }
            // Поток закончился: соединение закрыто или сервер не Linux.
            if let Some(inner) = weak.upgrade() {
                inner.root.set_visible(false);
            }
        });
    }

    pub fn stop(&self) {
        self.inner.stream.borrow_mut().take();
        self.inner.root.set_visible(false);
    }
}

impl Inner {
    fn show(&self, block: &[String]) {
        let Some(sample) = Sample::parse(block) else {
            return;
        };
        let usage = self
            .previous
            .borrow()
            .as_ref()
            .and_then(|previous| Usage::between(previous, &sample));

        match &usage {
            Some(usage) => {
                self.cpu.show(
                    &tr!("{}%", format!("{:.0}", usage.cpu_percent)),
                    Some(usage.cpu_percent),
                );
                self.network.set_text(&tr!(
                    "↓ {}/s  ↑ {}/s",
                    format_size(usage.rx_per_sec as u64),
                    format_size(usage.tx_per_sec as u64)
                ));
            }
            None => {
                self.cpu.show("…", None);
                self.network.set_text("…");
            }
        }

        self.memory.show(
            &format!(
                "{} / {}",
                format_size(sample.mem_used_kb() * 1024),
                format_size(sample.mem_total_kb * 1024)
            ),
            (sample.mem_total_kb > 0)
                .then(|| sample.mem_used_kb() as f64 * 100.0 / sample.mem_total_kb as f64),
        );
        match sample.disk_percent() {
            Some(percent) => self
                .disk
                .show(&tr!("{}%", format!("{percent:.0}")), Some(percent)),
            None => self.disk.show("—", None),
        }
        let load = format!("{:.2}", sample.load1)
            .replace('.', &crate::i18n::decimal_separator().to_string());
        self.load.set_text(&load);
        self.uptime
            .set_text(&sysstat::format_uptime(sample.uptime_secs));

        *self.previous.borrow_mut() = Some(sample);
        self.root.set_visible(true);
    }
}

/// Показатель в процентах: значение и мини-полоска рядом с ним.
struct Gauge {
    value: gtk::Label,
    bar: gtk::ProgressBar,
}

impl Gauge {
    /// `percent` — заполнение полоски; `None` оставляет её пустой.
    fn show(&self, text: &str, percent: Option<f64>) {
        self.value.set_text(text);
        let percent = percent.unwrap_or(0.0);
        self.bar.set_fraction((percent / 100.0).clamp(0.0, 1.0));
        mark_level(&self.value, percent);
        mark_level(&self.bar, percent);
    }
}

/// Подсвечивает значение, когда оно близко к пределу.
fn mark_level(widget: &impl IsA<gtk::Widget>, percent: f64) {
    widget.remove_css_class("warning");
    widget.remove_css_class("error");
    if percent >= CRITICAL_PERCENT {
        widget.add_css_class("error");
    } else if percent >= WARN_PERCENT {
        widget.add_css_class("warning");
    }
}
