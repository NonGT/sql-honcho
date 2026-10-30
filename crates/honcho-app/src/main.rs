mod editor_view;
mod find;
mod theme;
mod workspace;

use std::path::PathBuf;

use gpui::{
    px, size, App, AppContext, Application, Bounds, Global, Menu, MenuItem, Pixels, TitlebarOptions,
    WindowBounds, WindowOptions,
};

use editor_view as ed;
use workspace::Workspace;

pub struct FontSize(pub Pixels);

impl Global for FontSize {}

fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "sql-honcho".into(),
            items: vec![MenuItem::action("Quit sql-honcho", workspace::Quit)],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New File", workspace::NewFile),
                MenuItem::action("Open…", workspace::OpenFile),
                MenuItem::separator(),
                MenuItem::action("Save", workspace::Save),
                MenuItem::action("Save As…", workspace::SaveAs),
                MenuItem::separator(),
                MenuItem::action("Close Tab", workspace::CloseTab),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::action("Undo", ed::Undo),
                MenuItem::action("Redo", ed::Redo),
                MenuItem::separator(),
                MenuItem::action("Cut", ed::Cut),
                MenuItem::action("Copy", ed::Copy),
                MenuItem::action("Paste", ed::Paste),
                MenuItem::separator(),
                MenuItem::action("Find", find::Find),
                MenuItem::action("Replace", find::FindReplace),
                MenuItem::action("Find Next", find::FindNext),
                MenuItem::action("Find Previous", find::FindPrev),
                MenuItem::separator(),
                MenuItem::action("Toggle Line Comment", ed::ToggleComment),
                MenuItem::action("Indent", ed::Indent),
                MenuItem::action("Outdent", ed::Outdent),
            ],
        },
        Menu {
            name: "Selection".into(),
            items: vec![
                MenuItem::action("Select All", ed::SelectAll),
                MenuItem::action("Select Line", ed::SelectLine),
                MenuItem::separator(),
                MenuItem::action("Copy Line Up", ed::CopyLinesUp),
                MenuItem::action("Copy Line Down", ed::CopyLinesDown),
                MenuItem::action("Move Line Up", ed::MoveLinesUp),
                MenuItem::action("Move Line Down", ed::MoveLinesDown),
                MenuItem::action("Delete Line", ed::DeleteLines),
                MenuItem::separator(),
                MenuItem::action("Add Cursor Above", ed::AddCursorAbove),
                MenuItem::action("Add Cursor Below", ed::AddCursorBelow),
                MenuItem::action("Add Cursors to Line Ends", ed::SplitSelectionIntoLines),
                MenuItem::action("Add Next Occurrence", ed::SelectNextOccurrence),
                MenuItem::action("Select All Occurrences", ed::SelectAllOccurrences),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Zoom In", workspace::ZoomIn),
                MenuItem::action("Zoom Out", workspace::ZoomOut),
                MenuItem::action("Reset Zoom", workspace::ZoomReset),
                MenuItem::separator(),
                MenuItem::action("Next Tab", workspace::NextTab),
                MenuItem::action("Previous Tab", workspace::PrevTab),
            ],
        },
    ]
}

fn main() {
    let paths: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();

    Application::new().run(move |cx: &mut App| {
        cx.set_global(FontSize(px(theme::FONT_SIZE)));
        ed::bind_keys(cx);
        find::bind_keys(cx);
        workspace::bind_keys(cx);
        cx.set_menus(menus());
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1120.), px(780.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("sql-honcho".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Workspace::new(paths, window, cx)),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
