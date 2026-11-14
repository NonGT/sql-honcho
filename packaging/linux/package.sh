#!/bin/sh
# Builds a tarball, an AppImage and a .deb for this machine's
# architecture into dist/. Needs dpkg-deb, and curl for appimagetool.
set -eu
cd "$(dirname "$0")/../.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
arch=$(uname -m)
case "$arch" in
  x86_64) deb_arch=amd64 ;;
  aarch64) deb_arch=arm64 ;;
  *) echo "unsupported architecture $arch" >&2; exit 1 ;;
esac

cargo build --release --locked -p sql-honcho
bin=target/release/sql-honcho
rm -rf dist && mkdir -p dist

# the usual /usr layout, shared by all three
root=$(mktemp -d)/root
install -Dm755 "$bin" "$root/usr/bin/sql-honcho"
install -Dm644 packaging/linux/sql-honcho.desktop "$root/usr/share/applications/sql-honcho.desktop"
for s in 16 24 32 48 64 128 256 512; do
  install -Dm644 "assets/icon/png/$s.png" "$root/usr/share/icons/hicolor/${s}x${s}/apps/sql-honcho.png"
done
install -Dm644 assets/icon/icon.svg "$root/usr/share/icons/hicolor/scalable/apps/sql-honcho.svg"
install -Dm644 LICENSE "$root/usr/share/doc/sql-honcho/copyright"

name=sql-honcho-$version-linux-$arch
mkdir -p "dist/$name"
cp -R "$root/usr/." "dist/$name/"
cp README.md LICENSE "dist/$name/"
tar -C dist -czf "dist/$name.tar.gz" "$name"
rm -rf "dist/$name"

appdir=$(mktemp -d)/sql-honcho.AppDir
cp -R "$root" "$appdir"
ln -s usr/bin/sql-honcho "$appdir/AppRun"
cp packaging/linux/sql-honcho.desktop "$appdir/"
cp assets/icon/png/256.png "$appdir/sql-honcho.png"
tool=$(mktemp -d)/appimagetool
curl -fsSL -o "$tool" \
  "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$arch.AppImage"
chmod +x "$tool"
ARCH=$arch "$tool" --appimage-extract-and-run "$appdir" "dist/sql-honcho-$version-linux-$arch.AppImage"

mkdir -p "$root/DEBIAN"
size=$(du -sk "$root/usr" | cut -f1)
cat > "$root/DEBIAN/control" <<CONTROL
Package: sql-honcho
Version: $version
Architecture: $deb_arch
Maintainer: NonGT <NonGT@users.noreply.github.com>
Installed-Size: $size
Depends: libc6, libfreetype6, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libvulkan1
Recommends: libwayland-client0, libfontconfig1, xdg-desktop-portal-gtk | xdg-desktop-portal, gnome-keyring | keepassxc
Section: database
Priority: optional
Homepage: https://github.com/NonGT/sql-honcho
Description: SQL editor and database client
 A native SQL editor and client for PostgreSQL, MySQL/MariaDB,
 SQL Server and SQLite.
CONTROL
dpkg-deb --root-owner-group --build "$root" "dist/sql-honcho_${version}_$deb_arch.deb"
