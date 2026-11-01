# sql-honcho

A native SQL editor and database client written in Rust on top of
[gpui](https://crates.io/crates/gpui).

The goal is a fast editor that behaves like VS Code (multi-cursor, line
operations, the usual keybindings) plus a client for PostgreSQL, MySQL/MariaDB,
SQL Server and SQLite. So far it has the editor, saved connections, and
running queries against MySQL/MariaDB with results in a grid.

## Build

    cargo run --release -- path/to/file.sql

Runs on macOS, Windows and Linux. On macOS gpui is built with
`runtime_shaders`, so the Xcode Metal toolchain isn't needed. On Linux you
need the usual gpui packages, e.g. on Debian/Ubuntu:

    sudo apt install pkg-config libxkbcommon-dev libxkbcommon-x11-dev \
      libwayland-dev libx11-xcb-dev libxcb1-dev libfontconfig-dev \
      libfreetype-dev libvulkan-dev libssl-dev libzstd-dev

Connections can go through an SSH server (password, key file or ssh-agent;
`~/.ssh/config` aliases work) and use TLS from "prefer" up to full
certificate and host name checks. The first time an SSH server is seen, the
app shows its host key fingerprint and asks before adding it to
`~/.ssh/known_hosts`.

Passwords are kept in the OS keyring. On Linux that is the Secret Service,
which GNOME Keyring or KeePassXC provide; without one, connections still
work but their passwords can't be saved.

File dialogs on Linux go through xdg-desktop-portal, so a desktop without
one (plain XFCE, for example) needs `xdg-desktop-portal-gtk` installed.

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
    Cmd+Shift+C / Ctrl+Shift+C   connections
    Cmd+B / Ctrl+B               show or hide the object browser
    Ctrl+Space, Cmd+I / Ctrl+I   completion (it also opens while typing a name)
    Cmd+Enter / Ctrl+Enter       run the selection, or the statement at the cursor
    Cmd+Shift+Enter              run the whole tab (Ctrl+Shift+Enter)
    Cmd+. / Ctrl+.               cancel the running query

The object browser on the left lists every saved connection down to
columns and indexes, loading each level as it opens. Its filter searches the
server, so it finds tables that were never expanded. Right-click for select
top 1000, the definition, or SELECT/INSERT/UPDATE templates; double-click a
table to see its rows.

Opening a procedure, function, trigger or view from the browser gives a
script that replaces it, and an Apply button. Apply first checks the server
copy hasn't changed since the tab got it. For MySQL routines, which have to
be dropped and created again, it creates the new version under a throwaway
name first, so a script that won't create (a typo, a DEFINER you can't use)
stops before anything is dropped. Run… asks for a routine's parameters and
runs the CALL in a new tab. In the browser filter, `body:` searches routine
code, e.g. `body:orders`.

In the results grid: drag to select, click a header or row number for the
whole column or row, Cmd/Ctrl+C copies as TSV for spreadsheets, Enter or a
double-click opens the full value, and the arrow in a header sorts.
Cmd/Ctrl+J hides the results.

Completion knows tables, views, columns, routines and databases, follows
aliases through joins and subqueries in FROM, and matches fuzzily. Tab takes
the highlighted item; Enter only does after picking one with the arrows, so
Enter at the end of a word still starts a new line. Keywords come in the case
you're typing in.

The statement Cmd+Enter would run is marked in the gutter whenever the tab
is connected. On a connection tagged red, UPDATE or DELETE without WHERE asks
first.

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

MySQL and MariaDB serve TLS with a test CA from `docker/tls` (regenerate
with `docker/tls/gen.sh`); its certificate names 127.0.0.1 and
db.honcho.test, not localhost, so the tests can see a name check fail.

An `sshd` container (port 32222, user `tunnel`, password `Honcho_test1`, keys
in `docker/ssh`) reaches the databases by their service names, for the
tunnel tests.

SQL Server's image is amd64 only, so on Apple Silicon it runs under Docker
Desktop's emulation and takes a while to start.

## Layout

`crates/honcho-core` holds the text buffer, selections and SQL lexer with no UI
dependency. `crates/honcho-db` is the database layer: the driver interface, a
background runtime for sessions, the conformance suite every driver must
pass, and the drivers themselves (MySQL/MariaDB so far). `crates/honcho-config`
keeps saved connections in the platform config dir and their passwords in the
OS keyring (on Linux that means a Secret Service such as GNOME Keyring). `crates/honcho-app` is the gpui app. `docker/` has the test databases.
