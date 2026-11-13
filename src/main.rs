//! sshdeck — SSH-клиент с SFTP-деревом файлов и терминалом в одном окне.
//!
//! Запуск: `sshdeck [хост…]` — перечисленные хосты сразу открываются во
//! вкладках.

// Первым: макросы tr!/trn!/trc!/n_! видны во всех модулях ниже.
#[macro_use]
mod i18n;

mod connections;
mod exchange;
mod db;
mod entry;
mod forward;
mod keysetup;
mod localdirs;
mod localfs;
mod probe;
mod remote;
mod session;
mod settings;
mod sftp;
mod ssh_config;
mod sysstat;
mod ui;

use gtk::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "dev.sshdeck.App";

fn main() -> glib::ExitCode {
    // Язык выбирается до запуска GTK и любых потоков (см. `i18n::init`).
    i18n::init(&settings::Settings::load().language);
    let targets: Vec<String> = std::env::args().skip(1).collect();

    // NON_UNIQUE: каждый запуск — самостоятельный процесс со своими сокетами.
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| ui::window::build(app, &targets));

    // Аргументы разобраны выше; GTK их получать не должен.
    app.run_with_args::<&str>(&[])
}
