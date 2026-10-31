# sql-honcho

A native SQL editor and database client written in Rust on top of
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

On Windows you need the Visual Studio C++ build tools and a Windows SDK. On
ARM64 Windows you also need clang on PATH (`winget install LLVM.LLVM`) for
the `ring` dependency. If a release build fails running `fxc.exe`, point
`GPUI_FXC_PATH` at the arm64 one, e.g.
`C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\arm64\fxc.exe`.

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

## Tests

    cargo test --workspace

That covers unit tests, property tests for the editor core
(`crates/honcho-core/tests/properties.rs`), headless UI tests driven through
the real keymap, and the driver conformance suite against the fake driver.

Drivers also run the conformance suite against real databases in Docker
(MySQL 8.4, MariaDB 11.4, Postgres 16, SQL Server 2022):

    docker/up.sh            # start and seed everything, or e.g. `docker/up.sh mysql`
    docker/check-seeds.sh   # sanity-check the seed in each database
    HONCHO_IT=1 cargo test -p honcho-db
    docker compose -f docker/compose.yml down -v

SQL Server's image is amd64 only, so on Apple Silicon it runs under Docker
Desktop's emulation and takes a while to start.

## Layout

`crates/honcho-core` holds the text buffer, selections and SQL lexer with no UI
dependency. `crates/honcho-db` is the database layer: the driver interface, a
background runtime for sessions, and the conformance suite every driver must
pass. `crates/honcho-app` is the gpui app. `docker/` has the test databases.
