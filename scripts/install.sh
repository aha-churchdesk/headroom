#!/bin/bash
# Install Headroom from this checkout: Headroom.app in /Applications and the
# `headroom` CLI on your PATH. Run it again after pulling to upgrade.
#
#   ./scripts/install.sh                 build, install, start the menu bar app
#   ./scripts/install.sh --login         ...and start it at login
#   ./scripts/install.sh --hooks         ...and route agents' heavy commands through it
#   ./scripts/install.sh --bin DIR       put the CLI link in DIR (default ~/.local/bin)
#   ./scripts/install.sh --uninstall     remove the app, the link, the login item and the hooks
#
# Your shell config is never edited; if DIR isn't on PATH you get the line to add.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN_DIR="$HOME/.local/bin"
LOGIN=0 HOOKS=0 UNINSTALL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --login) LOGIN=1 ;;
    --hooks) HOOKS=1 ;;
    --uninstall) UNINSTALL=1 ;;
    --bin) BIN_DIR="${2:?--bin needs a directory}"; shift ;;
    -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (see --help)"; exit 2 ;;
  esac
  shift
done

APP="/Applications/Headroom.app"
CLI="$APP/Contents/MacOS/headroom"
LINK="$BIN_DIR/headroom"

if [ $UNINSTALL -eq 1 ]; then
  # Hooks first, while the CLI that removes them is still there: a hook
  # pointing at a missing binary would fail on every shell command.
  if [ -x "$CLI" ]; then "$CLI" hook uninstall; fi
  ./scripts/login-item.sh uninstall >/dev/null
  pkill -f 'Headroom.app/Contents/MacOS/headroomd' 2>/dev/null || true
  rm -rf "$APP"
  # Only our own link: never a headroom that something else put there.
  if [ -L "$LINK" ] && [ "$(readlink "$LINK")" = "$CLI" ]; then rm "$LINK"; fi
  echo "removed $APP and $LINK"
  echo "kept your data: ~/Library/Application Support/dev.headroom (delete it to forget learned profiles)"
  exit 0
fi

if command -v brew >/dev/null 2>&1 && brew list --formula headroom >/dev/null 2>&1; then
  echo "Headroom is also installed with Homebrew; two copies would each run a queue."
  echo "Remove one first: brew uninstall headroom, or ./scripts/install.sh --uninstall"
  exit 1
fi

./scripts/bundle.sh --install

mkdir -p "$BIN_DIR"
if [ -e "$LINK" ] && ! [ -L "$LINK" ]; then
  echo "$LINK exists and isn't a link; leaving it. Remove it, or pass --bin DIR."
  exit 1
fi
ln -sfn "$CLI" "$LINK"
echo "linked $LINK -> $CLI"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "$BIN_DIR isn't on your PATH. Add this to your shell config:"
     echo "  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

if [ $LOGIN -eq 1 ]; then
  # Starts it now too.
  ./scripts/login-item.sh install
else
  open -a "$APP"
  echo "started Headroom; ./scripts/install.sh --login starts it at login"
fi

if [ $HOOKS -eq 1 ]; then
  "$LINK" hook install
else
  echo "next: headroom hook install   (see the diff first with --dry-run)"
fi
