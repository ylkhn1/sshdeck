//! Элемент каталога — общий для удалённого (SFTP) и локального дерева
//! файлов. Модуль не зависит ни от GTK, ни от SFTP.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    /// Сокеты, устройства, битые ссылки и прочее, что нельзя ни открыть,
    /// ни раскрыть.
    Other,
}

/// Элемент каталога на сервере или на этом компьютере. Имя и путь
/// хранятся байтами, как их прислал сервер или вернула файловая система:
/// они не обязаны быть UTF-8. Для показа есть [`FileEntry::display_name`]
/// и `sftp::display`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: OsString,
    /// Полный путь (на сервере или локальный).
    pub path: PathBuf,
    /// Тип с учётом перехода по ссылке.
    pub kind: EntryKind,
    pub is_link: bool,
    pub size: Option<u64>,
    /// Время изменения, секунды Unix.
    pub modified: Option<u64>,
    /// Права доступа (биты `0o7777`, без типа файла), если известны.
    pub mode: Option<u32>,
}

impl FileEntry {
    /// Имя для показа: недопустимые в UTF-8 байты заменяются на «�».
    pub fn display_name(&self) -> String {
        self.name.to_string_lossy().into_owned()
    }

    /// Скрытый файл (имя начинается с точки).
    pub fn is_hidden(&self) -> bool {
        self.name.as_bytes().starts_with(b".")
    }
}
