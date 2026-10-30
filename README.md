# sql-honcho

A native SQL editor and database client for macOS, written in Rust on top of
[gpui](https://crates.io/crates/gpui).

The goal is a fast editor that behaves like VS Code (multi-cursor, line
operations, the usual keybindings) plus a client for PostgreSQL, MySQL/MariaDB,
SQL Server and SQLite. Right now only the editor exists; database connections
come next.

## Build

    cargo run --release -- path/to/file.sql

gpui is built with `runtime_shaders`, so the Xcode Metal toolchain isn't needed.

## Editor keys

    Cmd+D              select word, then add next occurrence
    Cmd+Shift+L        select all occurrences
    Alt+Cmd+Up/Down    add cursor above/below
    Alt+Click          add/remove a cursor
    Shift+Alt+Drag     box (column) selection
    Shift+Alt+I        cursor at the end of each selected line
    Alt+Up/Down        move lines
    Shift+Alt+Up/Down  copy lines
    Cmd+Shift+K        delete line
    Cmd+/              toggle -- comment
    Cmd+L              select line
    Cmd+Enter          insert line below
    Cmd+= / Cmd+-      zoom

Copy with no selection copies the whole line. Pasting N lines into N cursors
puts one line at each cursor.

## Layout

`crates/honcho-core` holds the text buffer, selections and SQL lexer with no UI
dependency, so it's tested with plain `cargo test`. `crates/honcho-app` is the
gpui app.
