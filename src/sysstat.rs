//! Показатели сервера для строки состояния: CPU, память, диск, сеть,
//! аптайм. Данные берутся из `/proc` (то есть только с Linux-серверов).

/// Раз в несколько секунд печатает блок показателей между `#begin` и `#end`.
/// Без одинарных кавычек: передаётся в `sh -c '…'`.
pub const SCRIPT: &str = r##"while :; do echo "#begin"; echo "#cpu"; head -n 1 /proc/stat; echo "#mem"; grep -E "^(MemTotal|MemAvailable):" /proc/meminfo; echo "#uptime"; cat /proc/uptime; echo "#load"; cat /proc/loadavg; echo "#disk"; df -Pk / | tail -n 1; echo "#net"; cat /proc/net/dev; echo "#end"; sleep 3; done 2>/dev/null"##;

pub const BLOCK_BEGIN: &str = "#begin";
pub const BLOCK_END: &str = "#end";

/// Один снимок счётчиков.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    cpu_total: u64,
    cpu_idle: u64,
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
    pub uptime_secs: f64,
    pub load1: f64,
    pub disk_used_kb: u64,
    pub disk_available_kb: u64,
    rx_bytes: u64,
    tx_bytes: u64,
}

impl Sample {
    /// Разбирает строки одного блока (без `#begin`/`#end`). `None`, если
    /// нет обязательных разделов — например, сервер не Linux.
    pub fn parse<S: AsRef<str>>(lines: &[S]) -> Option<Self> {
        let mut section = "";
        let mut cpu = None;
        let mut mem_total = None;
        let mut mem_available = None;
        let mut uptime = None;
        let mut load1 = 0.0;
        let mut disk = None;
        let (mut rx, mut tx) = (0u64, 0u64);

        for line in lines {
            let line = line.as_ref().trim();
            if let Some(name) = line.strip_prefix('#') {
                section = match name {
                    "cpu" => "cpu",
                    "mem" => "mem",
                    "uptime" => "uptime",
                    "load" => "load",
                    "disk" => "disk",
                    "net" => "net",
                    _ => "",
                };
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            match section {
                "cpu" if fields.first() == Some(&"cpu") => {
                    let values: Vec<u64> = fields[1..]
                        .iter()
                        .take(8)
                        .filter_map(|v| v.parse().ok())
                        .collect();
                    if values.len() >= 4 {
                        let idle = values[3] + values.get(4).copied().unwrap_or(0);
                        cpu = Some((values.iter().sum::<u64>(), idle));
                    }
                }
                "mem" => {
                    let value = fields.get(1).and_then(|v| v.parse::<u64>().ok());
                    match fields.first() {
                        Some(&"MemTotal:") => mem_total = value,
                        Some(&"MemAvailable:") => mem_available = value,
                        _ => {}
                    }
                }
                "uptime" => uptime = fields.first().and_then(|v| v.parse::<f64>().ok()),
                "load" => {
                    load1 = fields
                        .first()
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(0.0);
                }
                "disk" if fields.len() >= 4 => {
                    let used = fields[2].parse::<u64>().ok();
                    let available = fields[3].parse::<u64>().ok();
                    if let (Some(used), Some(available)) = (used, available) {
                        disk = Some((used, available));
                    }
                }
                "net" => {
                    // «  eth0: rx_bytes … (8 полей) tx_bytes …»; заголовки
                    // таблицы двоеточия с числами не содержат.
                    if let Some((name, rest)) = line.split_once(':') {
                        let values: Vec<u64> = rest
                            .split_whitespace()
                            .filter_map(|v| v.parse().ok())
                            .collect();
                        if name.trim() != "lo" && values.len() >= 9 {
                            rx += values[0];
                            tx += values[8];
                        }
                    }
                }
                _ => {}
            }
        }

        let (cpu_total, cpu_idle) = cpu?;
        let (disk_used_kb, disk_available_kb) = disk.unwrap_or((0, 0));
        Some(Self {
            cpu_total,
            cpu_idle,
            mem_total_kb: mem_total?,
            mem_available_kb: mem_available.unwrap_or(0),
            uptime_secs: uptime?,
            load1,
            disk_used_kb,
            disk_available_kb,
            rx_bytes: rx,
            tx_bytes: tx,
        })
    }

    pub fn mem_used_kb(&self) -> u64 {
        self.mem_total_kb.saturating_sub(self.mem_available_kb)
    }

    /// Заполненность корневого раздела в процентах.
    pub fn disk_percent(&self) -> Option<f64> {
        let total = self.disk_used_kb + self.disk_available_kb;
        (total > 0).then(|| self.disk_used_kb as f64 * 100.0 / total as f64)
    }
}

/// Скорости, которые получаются из двух соседних снимков.
#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    pub cpu_percent: f64,
    pub rx_per_sec: f64,
    pub tx_per_sec: f64,
}

impl Usage {
    /// `None`, если снимки не по порядку (сервер перезагрузился) или между
    /// ними не прошло времени.
    pub fn between(previous: &Sample, current: &Sample) -> Option<Self> {
        let seconds = current.uptime_secs - previous.uptime_secs;
        let total = current.cpu_total.checked_sub(previous.cpu_total)?;
        let idle = current.cpu_idle.checked_sub(previous.cpu_idle)?;
        if seconds <= 0.0 || total == 0 {
            return None;
        }
        let rate = |now: u64, before: u64| now.saturating_sub(before) as f64 / seconds;
        Some(Self {
            cpu_percent: (100.0 * (1.0 - idle as f64 / total as f64)).clamp(0.0, 100.0),
            rx_per_sec: rate(current.rx_bytes, previous.rx_bytes),
            tx_per_sec: rate(current.tx_bytes, previous.tx_bytes),
        })
    }
}

/// Аптайм двумя старшими единицами: `12 d 4 h`, `3 h 20 min`, `45 min`.
pub fn format_uptime(seconds: f64) -> String {
    let minutes = (seconds.max(0.0) / 60.0) as u64;
    let (days, hours, mins) = (minutes / 1440, minutes % 1440 / 60, minutes % 60);
    if days > 0 {
        tr!("{} d {} h", days, hours)
    } else if hours > 0 {
        tr!("{} h {} min", hours, mins)
    } else {
        tr!("{} min", mins)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(cpu: &str, uptime: &str, rx: u64, tx: u64) -> Vec<String> {
        format!(
            "#cpu\n{cpu}\n#mem\nMemTotal:       16000000 kB\nMemAvailable:   12000000 kB\n\
             #uptime\n{uptime} 999.0\n#load\n0.42 0.30 0.20 1/300 4242\n\
             #disk\n/dev/sda1 100000 40000 60000 40% /\n#net\n\
             Inter-|   Receive                                                |  Transmit\n \
             face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets\n    \
             lo: 500 5 0 0 0 0 0 0 500 5 0 0 0 0 0 0\n  \
             eth0: {rx} 10 0 0 0 0 0 0 {tx} 10 0 0 0 0 0 0\n"
        )
        .lines()
        .map(str::to_string)
        .collect()
    }

    #[test]
    fn script_has_no_single_quotes() {
        assert!(!SCRIPT.contains('\''));
    }

    #[test]
    fn block_is_parsed() {
        let sample =
            Sample::parse(&block("cpu  100 0 50 800 50 0 0 0 0 0", "3600.5", 1000, 2000))
                .expect("sample");
        assert_eq!(sample.mem_total_kb, 16_000_000);
        assert_eq!(sample.mem_used_kb(), 4_000_000);
        assert_eq!(sample.uptime_secs, 3600.5);
        assert_eq!(sample.load1, 0.42);
        assert_eq!(sample.disk_percent(), Some(40.0));
        // lo не учитывается.
        assert_eq!((sample.rx_bytes, sample.tx_bytes), (1000, 2000));
    }

    #[test]
    fn usage_comes_from_two_samples() {
        let before =
            Sample::parse(&block("cpu  100 0 50 800 50 0 0 0", "100.0", 1000, 2000)).expect("a");
        // +200 тиков, из них 150 простоя → 25 % загрузки; 2 секунды.
        let after =
            Sample::parse(&block("cpu  140 0 60 940 60 0 0 0", "102.0", 5000, 2600)).expect("b");
        let usage = Usage::between(&before, &after).expect("usage");
        assert!((usage.cpu_percent - 25.0).abs() < 1e-9);
        assert_eq!(usage.rx_per_sec, 2000.0);
        assert_eq!(usage.tx_per_sec, 300.0);
        // Снимки в обратном порядке (перезагрузка) скоростей не дают.
        assert_eq!(Usage::between(&after, &before), None);
    }

    #[test]
    fn foreign_output_is_rejected() {
        assert_eq!(Sample::parse(&["#cpu", "#mem", "#uptime"]), None);
        assert_eq!(Sample::parse::<&str>(&[]), None);
    }

    #[test]
    fn uptime_text() {
        assert_eq!(format_uptime(59.0), "0 min");
        assert_eq!(format_uptime(45.0 * 60.0), "45 min");
        assert_eq!(format_uptime(3.0 * 3600.0 + 20.0 * 60.0), "3 h 20 min");
        assert_eq!(format_uptime(12.0 * 86400.0 + 4.0 * 3600.0), "12 d 4 h");
    }
}
