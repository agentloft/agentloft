#!/bin/sh
# Install Agentloft from this folder for the current user:
#   ~/.local/share/agentloft (the binaries and the web client),
#   ~/.local/bin/agentloft and agentloftd (links), the menu entry and icons.
# sudo PREFIX=/usr/local ./install.sh installs for everyone instead (sudo drops
# PREFIX from the environment when it comes first).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
prefix=${PREFIX:-$HOME/.local}
dest="$prefix/share/agentloft"

# A running daemon keeps the old binary; it is replaced when the app next
# starts (it asks an outdated daemon to make way).
rm -rf "$dest.new"
mkdir -p "$dest.new"
cp -R "$here/bin" "$here/web" "$dest.new/"
rm -rf "$dest.old"
[ -d "$dest" ] && mv "$dest" "$dest.old"
mv "$dest.new" "$dest"
rm -rf "$dest.old"

mkdir -p "$prefix/bin"
ln -sf "$dest/bin/agentloft" "$prefix/bin/agentloft"
ln -sf "$dest/bin/agentloftd" "$prefix/bin/agentloftd"

mkdir -p "$prefix/share/applications"
sed "s|^Exec=agentloft|Exec=$dest/bin/agentloft|" \
  "$here/share/applications/dev.agentloft.Agentloft.desktop" \
  > "$prefix/share/applications/dev.agentloft.Agentloft.desktop"
for icon in "$here"/share/icons/hicolor/*/apps/dev.agentloft.Agentloft.png; do
  size=$(basename "$(dirname "$(dirname "$icon")")")
  mkdir -p "$prefix/share/icons/hicolor/$size/apps"
  cp "$icon" "$prefix/share/icons/hicolor/$size/apps/"
done
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$prefix/share/applications" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q "$prefix/share/icons/hicolor" 2>/dev/null || true

echo "Installed Agentloft in $dest"
case ":$PATH:" in
  *":$prefix/bin:"*) echo "Run it from your app menu, or: agentloft" ;;
  *) echo "Run it from your app menu, or: $prefix/bin/agentloft ($prefix/bin is not on your PATH)" ;;
esac
