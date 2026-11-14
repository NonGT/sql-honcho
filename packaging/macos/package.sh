#!/bin/sh
# Builds a universal sql-honcho.app and a .dmg of it into dist/.
# Not signed with a Developer ID yet, so it's signed ad hoc: the first
# launch needs right-click > Open.
set -eu
cd "$(dirname "$0")/../.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

for target in aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build --release --locked -p sql-honcho --target "$target"
done

app=dist/sql-honcho.app
rm -rf dist && mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create -output "$app/Contents/MacOS/sql-honcho" \
  target/aarch64-apple-darwin/release/sql-honcho \
  target/x86_64-apple-darwin/release/sql-honcho
cp assets/icon/sql-honcho.icns "$app/Contents/Resources/"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
codesign --force --sign - "$app"

stage=$(mktemp -d)
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
hdiutil create -quiet -volname sql-honcho -srcfolder "$stage" -format UDZO \
  "dist/sql-honcho-$version-macos-universal.dmg"
rm -rf "$stage"
