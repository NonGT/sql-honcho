# sql-honcho

A native SQL editor and database client for macOS, written in Rust on top of
[gpui](https://crates.io/crates/gpui).

The goal is a fast editor that behaves like VS Code (multi-cursor, line
operations, the usual keybindings) plus a client for PostgreSQL, MySQL/MariaDB,
SQL Server and SQLite. Right now only the editor exists; database connections
come next.

## Build

    cargo run --release -- path/to/file.sql

Runs on macOS, Windows and Linux. On macOS gpui is built with
`runtime_shaders`, so the Xcode Metal toolchain isn't needed. On Linux you
need the usual gpui packages, e.g. on Debian/Ubuntu:

    sudo apt install pkg-config libxkbcommon-dev libxkbcommon-x11-dev \
      libwayland-dev libx11-xcb-dev libxcb1-dev libfontconfig-dev \
      libfreetype-dev libvulkan-dev libssl-dev libzstd-dev

## Editor keys

Bindings follow VS Code's defaults on each platform; the full tables are in
`crates/honcho-app/src/keymap.rs`. The main ones, macOS first:

    Cmd+D / Ctrl+D               select word, then add next occurrence
    Cmd+Shift+L / Ctrl+Shift+L   select all occurrences
    Alt+Cmd+Up/Down              add cursor above/below
                                 (Ctrl+Alt+Up/Down on Windows, Shift+Alt+Up/Down on Linux)
    Alt+Click                    add/remove a cursor
    Shift+Alt+Drag               box (column) selection
    Shift+Alt+I                  cursor at the end of each selected line
    Alt+Up/Down                  move lines
    Cmd+/ / Ctrl+/               toggle -- comment
    Cmd+F / Ctrl+F               find; Cmd+Alt+F / Ctrl+H replace

Copy with no selection copies the whole line. Pasting N lines into N cursors
puts one line at each cursor.

## Layout

`crates/honcho-core` holds the text buffer, selections and SQL lexer with no UI
dependency, so it's tested with plain `cargo test`. `crates/honcho-app` is the
gpui app.
