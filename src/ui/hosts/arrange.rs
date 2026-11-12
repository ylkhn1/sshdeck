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

    use super::{DropRow, Mark, drop_target, forget_paths, rebase_paths};
    use crate::connections::{FolderPath, Target};

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
}
