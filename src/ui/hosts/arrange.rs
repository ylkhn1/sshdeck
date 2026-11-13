//! Порядок и вложенность сохранённых подключений в боковой панели: куда
//! ляжет перетаскиваемая строка, пути свёрнутых папок после переносов,
//! список папок для выбора.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, pango};

use crate::connections::{FolderPath, Target, join_path};

/// Отступ строки на каждый уровень вложенности.
pub const INDENT: i32 = 18;

/// Классы строки, над которой держат перетаскиваемый элемент: черта сверху
/// или снизу. Перенос внутрь папки подсвечивает строку целиком
/// (`ListBox::drag_highlight_row`).
pub const DROP_BEFORE_CLASS: &str = "sshdeck-drop-before";
pub const DROP_AFTER_CLASS: &str = "sshdeck-drop-after";

const CSS: &str = "
.sshdeck-hosts > row.sshdeck-drop-before { box-shadow: inset 0 2px 0 0 var(--accent-bg-color); }
.sshdeck-hosts > row.sshdeck-drop-after { box-shadow: inset 0 -2px 0 0 var(--accent-bg-color); }
";

/// Подключает стили отметок перетаскивания (один раз на процесс).
pub fn install_css() {
    thread_local! {
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
    }
    if INSTALLED.with(|installed| installed.replace(true)) {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// Строка, над которой отпускают перетаскиваемый элемент.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropRow<'a> {
    Connection(&'a [usize]),
    Folder { loc: &'a [usize], collapsed: bool },
    /// Заголовок «Сохранённые»: в начало корня.
    Start,
    /// Заголовок следующего раздела: в конец корня.
    End,
}

/// Как показать место вставки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Above,
    Below,
    Inside,
}

/// Куда ляжет элемент, отпущенный на строке `row`. `fraction` — высота
/// указателя внутри строки (0 — верх, 1 — низ), `root_len` — сколько
/// элементов в корне. Возвращает `None`, если перенос ничего не изменит
/// или невозможен (папку — в неё саму).
pub fn drop_target(source: &[usize], row: DropRow, fraction: f64, root_len: usize) -> Option<(Target, Mark)> {
    let (target, mark) = match row {
        DropRow::Connection(loc) if fraction < 0.5 => (Target::Before(loc.to_vec()), Mark::Above),
        DropRow::Connection(loc) => (Target::After(loc.to_vec()), Mark::Below),
        DropRow::Folder { loc, .. } if fraction < 0.25 => (Target::Before(loc.to_vec()), Mark::Above),
        // Ниже раскрытой папки идёт её содержимое, поэтому «после папки»
        // предлагается только для свёрнутой.
        DropRow::Folder { loc, collapsed: true } if fraction > 0.75 => {
            (Target::After(loc.to_vec()), Mark::Below)
        }
        DropRow::Folder { loc, .. } => (Target::Into(loc.to_vec()), Mark::Inside),
        DropRow::Start if root_len > 0 => (Target::Before(vec![0]), Mark::Below),
        DropRow::Start => (Target::Into(Vec::new()), Mark::Below),
        DropRow::End => (Target::Into(Vec::new()), Mark::Above),
    };
    (!changes_nothing(source, &target)).then_some((target, mark))
}

/// Перенос на то же место или папки в саму себя.
fn changes_nothing(source: &[usize], target: &Target) -> bool {
    let Some((&index, parent)) = source.split_last() else {
        return true;
    };
    let sibling = |loc: &[usize], offset: isize| {
        loc.split_last()
            .is_some_and(|(&i, p)| p == parent && index.checked_add_signed(offset) == Some(i))
    };
    match target {
        Target::Before(anchor) => anchor.starts_with(source) || sibling(anchor, 1),
        Target::After(anchor) => anchor.starts_with(source) || sibling(anchor, -1),
        Target::Into(anchor) => anchor.starts_with(source),
    }
}

/// Папка `from` стала `to`: свёрнутые папки из её поддерева остаются
/// свёрнутыми под новым путём.
pub fn rebase_paths(set: &mut HashSet<FolderPath>, from: &[String], to: &[String]) {
    if from == to {
        return;
    }
    let moved: Vec<FolderPath> = set.iter().filter(|path| path.starts_with(from)).cloned().collect();
    for path in moved {
        set.remove(&path);
        let mut new = to.to_vec();
        new.extend_from_slice(&path[from.len()..]);
        set.insert(new);
    }
}

/// Забывает папку и всё её поддерево.
pub fn forget_paths(set: &mut HashSet<FolderPath>, prefix: &[String]) {
    set.retain(|path| !path.starts_with(prefix));
}

/// Папку `path` убрали, а содержимое подняли на уровень выше: сама она
/// забывается, вложенные папки остаются свёрнутыми под новым путём.
pub fn lift_paths(set: &mut HashSet<FolderPath>, path: &[String]) {
    let Some((_, parent)) = path.split_last() else {
        return;
    };
    set.remove(path);
    rebase_paths(set, path, parent);
}

/// Оставляет только пути папок, которые есть в списке `folders`: папки,
/// удалённые, убранные или опустевшие (ушло последнее подключение),
/// забываются, чтобы не копиться в настройках. Возвращает, изменилось ли
/// что-нибудь.
pub fn keep_existing(set: &mut HashSet<FolderPath>, folders: &[FolderPath]) -> bool {
    let before = set.len();
    let existing: HashSet<&FolderPath> = folders.iter().collect();
    set.retain(|path| existing.contains(path));
    set.len() != before
}

/// Путь для чтения: «Работа / Сервера».
pub fn display_path(path: &[String]) -> String {
    path.join(" / ")
}

/// Папка в списке выбора: путь и цвет.
pub struct FolderChoice {
    pub path: FolderPath,
    pub color: Option<String>,
}

/// Список папок деревом (с отступами); щелчок передаёт путь в том виде,
/// в каком его вводят в поле папки. `root_label` — строка «вне папок»
/// в начале списка (передаёт пустой путь).
pub fn folder_picker(
    folders: &[FolderChoice],
    root_label: Option<&str>,
    on_pick: impl Fn(String) + 'static,
) -> gtk::Box {
    let on_pick: Rc<dyn Fn(String)> = Rc::new(on_pick);
    let choices = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let add = |depth: usize, icon: &str, color: Option<&str>, label: &str, tooltip: Option<String>, value: String| {
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_start(depth as i32 * INDENT);
        let image = gtk::Image::from_icon_name(icon);
        let text = gtk::Label::builder()
            .label(label)
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .max_width_chars(30)
            .build();
        if let Some(color) = color {
            let class = format!("sshdeck-folder-{color}");
            image.add_css_class(&class);
            text.add_css_class(&class);
        }
        content.append(&image);
        content.append(&text);
        let button = gtk::Button::builder()
            .child(&content)
            .css_classes(["flat"])
            .build();
        button.set_tooltip_text(tooltip.as_deref());
        let on_pick = Rc::clone(&on_pick);
        button.connect_clicked(move |_| on_pick(value.clone()));
        choices.append(&button);
    };
    if let Some(label) = root_label {
        add(0, "go-home-symbolic", None, label, None, String::new());
    }
    for folder in folders {
        let Some(name) = folder.path.last() else { continue };
        add(
            folder.path.len() - 1,
            "folder-symbolic",
            folder.color.as_deref(),
            name,
            Some(display_path(&folder.path)),
            join_path(&folder.path),
        );
    }
    choices
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{DropRow, Mark, drop_target, forget_paths, keep_existing, lift_paths, rebase_paths};
    use crate::connections::{Connection, Connections, FolderPath, Target};

    fn p(text: &str) -> FolderPath {
        crate::connections::split_path(text)
    }

    #[test]
    fn drop_on_connection_goes_above_or_below() {
        let row = DropRow::Connection(&[1, 2]);
        assert_eq!(drop_target(&[0], row, 0.2, 3), Some((Target::Before(vec![1, 2]), Mark::Above)));
        assert_eq!(drop_target(&[0], row, 0.8, 3), Some((Target::After(vec![1, 2]), Mark::Below)));
    }

    #[test]
    fn drop_on_folder_goes_inside_or_beside() {
        let open = DropRow::Folder { loc: &[2], collapsed: false };
        let closed = DropRow::Folder { loc: &[2], collapsed: true };
        assert_eq!(drop_target(&[0], open, 0.1, 3), Some((Target::Before(vec![2]), Mark::Above)));
        assert_eq!(drop_target(&[0], open, 0.5, 3), Some((Target::Into(vec![2]), Mark::Inside)));
        assert_eq!(drop_target(&[0], open, 0.9, 3), Some((Target::Into(vec![2]), Mark::Inside)));
        assert_eq!(drop_target(&[0], closed, 0.9, 3), Some((Target::After(vec![2]), Mark::Below)));
    }

    #[test]
    fn headings_mean_start_and_end_of_the_root() {
        assert_eq!(drop_target(&[1, 0], DropRow::Start, 0.5, 2), Some((Target::Before(vec![0]), Mark::Below)));
        assert_eq!(drop_target(&[1, 0], DropRow::End, 0.5, 2), Some((Target::Into(vec![]), Mark::Above)));
    }

    #[test]
    fn useless_or_impossible_drops_are_refused() {
        // На себя, сразу перед следующим или после предыдущего соседа.
        assert_eq!(drop_target(&[1], DropRow::Connection(&[1]), 0.2, 3), None);
        assert_eq!(drop_target(&[1], DropRow::Connection(&[2]), 0.2, 3), None);
        assert_eq!(drop_target(&[1], DropRow::Connection(&[0]), 0.8, 3), None);
        assert_eq!(drop_target(&[0], DropRow::Start, 0.5, 3), None);
        // Папку — внутрь себя или во вложенную.
        assert_eq!(drop_target(&[1], DropRow::Folder { loc: &[1], collapsed: false }, 0.5, 3), None);
        assert_eq!(drop_target(&[1], DropRow::Connection(&[1, 0, 2]), 0.8, 3), None);
        // А соседняя папка — можно.
        assert!(drop_target(&[1], DropRow::Folder { loc: &[2], collapsed: false }, 0.5, 3).is_some());
    }

    #[test]
    fn collapsed_paths_follow_renames_and_removals() {
        let mut set: HashSet<FolderPath> = [p("Прод"), p("Прод/EU"), p("Продажи"), p("Тест")].into();
        rebase_paths(&mut set, &p("Прод"), &p("Архив/Прод2"));
        let expected: HashSet<FolderPath> = [p("Архив/Прод2"), p("Архив/Прод2/EU"), p("Продажи"), p("Тест")].into();
        assert_eq!(set, expected);
        forget_paths(&mut set, &p("Архив"));
        assert_eq!(set, [p("Продажи"), p("Тест")].into());
    }

    #[test]
    fn rebase_only_touches_the_subtree_not_namesakes_by_prefix() {
        // «Прод» — не префикс пути «Продажи»: сравниваются части, а не текст.
        let mut set: HashSet<FolderPath> = [p("Прод/EU"), p("Продажи/EU"), p("a\\/b/c")].into();
        rebase_paths(&mut set, &p("Прод"), &p("Архив"));
        rebase_paths(&mut set, &p("a\\/b"), &p("x/a\\/b"));
        assert_eq!(set, [p("Архив/EU"), p("Продажи/EU"), p("x/a\\/b/c")].into());
        // Перенос на то же место ничего не меняет.
        let before = set.clone();
        rebase_paths(&mut set, &p("Архив"), &p("Архив"));
        assert_eq!(set, before);
    }

    #[test]
    fn lifted_folder_is_forgotten_and_its_subfolders_move_up() {
        let mut set: HashSet<FolderPath> = [p("Прод"), p("Прод/EU"), p("Прод/EU/Резерв"), p("Тест")].into();
        lift_paths(&mut set, &p("Прод"));
        assert_eq!(set, [p("EU"), p("EU/Резерв"), p("Тест")].into());
        lift_paths(&mut set, &p("EU/Резерв"));
        assert_eq!(set, [p("EU"), p("Тест")].into());
        // Корень убрать нельзя.
        lift_paths(&mut set, &[]);
        assert_eq!(set, [p("EU"), p("Тест")].into());
    }

    #[test]
    fn only_existing_folders_are_kept() {
        let mut set: HashSet<FolderPath> = [p("Прод"), p("Прод/EU"), p("Удалённая"), p("Тест/Пусто")].into();
        let folders = vec![p("Прод"), p("Прод/EU"), p("Тест")];
        assert!(keep_existing(&mut set, &folders));
        assert_eq!(set, [p("Прод"), p("Прод/EU")].into());
        assert!(!keep_existing(&mut set, &folders));
        assert!(keep_existing(&mut set, &[]));
        assert!(set.is_empty());
    }

    /// Свёрнутые папки вслед за правками настоящего списка — так же, как
    /// это делает боковая панель: перенос и переименование — `rebase_paths`,
    /// «убрать папку» — `lift_paths`, удаление — `forget_paths`, после
    /// любой правки — `keep_existing`.
    #[test]
    fn collapsed_paths_follow_real_list_edits() {
        let mut list = Connections::default();
        list.add(Connection::new("web", "root@web"), &p("Прод"));
        list.add(Connection::new("fra1", "root@fra1"), &p("Прод/EU"));
        list.add(Connection::new("ams", "root@ams"), &p("Прод/EU/Резерв"));
        list.add(Connection::new("стенд", "root@стенд"), &p("Тест"));
        list.add(Connection::new("lab", "root@lab"), &p("a/b;c"));
        let mut set: HashSet<FolderPath> =
            [p("Прод"), p("Прод/EU/Резерв"), p("Тест"), p("a/b;c")].into();
        let sync = |set: &mut HashSet<FolderPath>, list: &Connections| {
            keep_existing(set, &list.folders());
        };

        // Переименование: «Прод» → «Production», поддерево следом.
        let loc = list.folder_loc(&p("Прод")).expect("Прод");
        let new = list.rename_folder(&loc, "Production").expect("rename");
        rebase_paths(&mut set, &p("Прод"), &list.folder_path(&new).expect("path"));
        sync(&mut set, &list);
        assert_eq!(set, [p("Production"), p("Production/EU/Резерв"), p("Тест"), p("a/b;c")].into());

        // Перенос «EU» в «Тест».
        let loc = list.folder_loc(&p("Production/EU")).expect("EU");
        let new = list.move_into(&loc, &p("Тест")).expect("move");
        rebase_paths(&mut set, &p("Production/EU"), &list.folder_path(&new).expect("path"));
        sync(&mut set, &list);
        assert_eq!(set, [p("Production"), p("Тест"), p("Тест/EU/Резерв"), p("a/b;c")].into());

        // «Убрать папку» «Тест»: содержимое поднимается в корень.
        let loc = list.folder_loc(&p("Тест")).expect("Тест");
        assert!(list.dissolve_folder(&loc));
        lift_paths(&mut set, &p("Тест"));
        sync(&mut set, &list);
        assert_eq!(set, [p("Production"), p("EU/Резерв"), p("a/b;c")].into());

        // Ушло последнее подключение — папка исчезла и забыта.
        let loc = list.find_connection(&Connection::new("ams", "root@ams")).expect("ams");
        list.remove(&loc);
        sync(&mut set, &list);
        assert_eq!(set, [p("Production"), p("a/b;c")].into());

        // Удаление папки вместе с содержимым.
        let loc = list.folder_loc(&p("a")).expect("a");
        list.delete_folder(&loc);
        forget_paths(&mut set, &p("a"));
        sync(&mut set, &list);
        assert_eq!(set, [p("Production")].into());
    }
}
