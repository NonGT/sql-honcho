mod browser;
mod catalog;
mod commands;
mod connection_dialog;
mod connections;
mod editor_view;
mod find;
mod grid;
mod keymap;
mod palette;
mod prompt;
mod query;

mod diff_view;
mod project_pane;
mod routine_form;
#[cfg(test)]
mod tests;
mod theme;
mod workspace;

use std::path::PathBuf;

use gpui::{
    px, size, App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds,
    WindowDecorations, WindowOptions,
};

use workspace::Workspace;

/// A bare binary has no bundle to take an icon from, so the Dock would show
/// the generic one.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(include_bytes!("../../../assets/icon/png/1024.png"));
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }
}

fn main() {
    let paths: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();

    Application::new().run(move |cx: &mut App| {
        theme::init_font(cx);
        keymap::install(cx);
        connections::Connections::init(cx);
        #[cfg(target_os = "macos")]
        set_dock_icon();
        if keymap::Platform::current() == keymap::Platform::Linux {
            cx.set_prompt_builder(prompt::build);
        }
        cx.set_menus(commands::native_menus());
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
                // gpui asks for server decorations by default, but GNOME and
                // weston don't draw any, which leaves the window without a title
                // bar. Only Linux looks at this.
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Workspace::new(paths, window, cx)),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
