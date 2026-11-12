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
    cpu: gtk::Label,
    memory: gtk::Label,
    disk: gtk::Label,
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
        let item = |title: &str, tooltip: &str| {
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
                    .css_classes(["dim-label"])
                    .build(),
            );
            cell.append(&value);
            row.append(&cell);
            value
        };
        let cpu = item("CPU", tr!("Server CPU usage"));
        let memory = item(tr!("RAM"), tr!("Memory in use out of the server’s total memory"));
        let disk = item(tr!("Disk /"), tr!("Root partition usage"));
        let network = item(tr!("Network"), tr!("Receive and send rates across all server interfaces"));
        let uptime = item(tr!("Uptime"), tr!("Time since the server was last rebooted"));
        let load = item(tr!("Load"), tr!("One-minute load average"));

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
                self.cpu
                    .set_text(&tr!("{}%", format!("{:.0}", usage.cpu_percent)));
                mark_level(&self.cpu, usage.cpu_percent);
                self.network.set_text(&tr!(
                    "↓ {}/s  ↑ {}/s",
                    format_size(usage.rx_per_sec as u64),
                    format_size(usage.tx_per_sec as u64)
                ));
            }
            None => {
                self.cpu.set_text("…");
                self.network.set_text("…");
            }
        }

        self.memory.set_text(&format!(
            "{} / {}",
            format_size(sample.mem_used_kb() * 1024),
            format_size(sample.mem_total_kb * 1024)
        ));
        if sample.mem_total_kb > 0 {
            mark_level(
                &self.memory,
                sample.mem_used_kb() as f64 * 100.0 / sample.mem_total_kb as f64,
            );
        }
        match sample.disk_percent() {
            Some(percent) => {
                self.disk.set_text(&tr!("{}%", format!("{percent:.0}")));
                mark_level(&self.disk, percent);
            }
            None => self.disk.set_text("—"),
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

/// Подсвечивает значение, когда оно близко к пределу.
fn mark_level(label: &gtk::Label, percent: f64) {
    label.remove_css_class("warning");
    label.remove_css_class("error");
    if percent >= CRITICAL_PERCENT {
        label.add_css_class("error");
    } else if percent >= WARN_PERCENT {
        label.add_css_class("warning");
    }
}
