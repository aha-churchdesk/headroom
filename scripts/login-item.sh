#!/bin/bash
# Start Headroom at login, via a LaunchAgent. The same plist the popover's
# "Start at login" switch writes; this also starts it now.
#
# A LaunchAgent rather than SMAppService: the app is ad-hoc signed, and
# SMAppService ties registration to the signing identity, so it would break
# on every rebuild. Homebrew users get the same from `brew services`.
#
#   ./scripts/login-item.sh install | uninstall | status
set -uo pipefail

LABEL="dev.headroom"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
APP="/Applications/Headroom.app/Contents/MacOS/headroomd"
DOMAIN="gui/$(id -u)"

case "${1:-status}" in
  install)
    if [ ! -x "$APP" ]; then
      echo "not installed: $APP"
      echo "run ./scripts/bundle.sh --install first"
      exit 1
    fi
    if [ -f "$HOME/Library/LaunchAgents/homebrew.mxcl.headroom.plist" ]; then
      echo "brew services already starts Headroom at login; use that, or 'brew services stop headroom' first"
      exit 1
    fi
    mkdir -p "$(dirname "$PLIST")"
    cat > "$PLIST" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>            <string>$LABEL</string>
  <key>ProgramArguments</key> <array><string>$APP</string></array>
  <key>RunAtLoad</key>        <true/>
  <key>KeepAlive</key>        <false/>
  <key>ProcessType</key>      <string>Interactive</string>
</dict>
</plist>
PL
    # Not KeepAlive: Quit should stay quit. Interactive: it draws a menu bar item.
    launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null
    pkill -f 'Headroom.app/Contents/MacOS/headroomd' 2>/dev/null
    sleep 1
    launchctl bootstrap "$DOMAIN" "$PLIST" || { echo "bootstrap failed"; exit 1; }
    echo "installed $PLIST"
    echo "starts at login, and has been started now"
    ;;

  uninstall)
    launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null
    rm -f "$PLIST"
    echo "removed $PLIST: it will no longer start at login"
    echo "(the app is untouched; open it with: open -a Headroom)"
    ;;

  status)
    if [ -f "$PLIST" ]; then echo "plist:   $PLIST"; else echo "plist:   not installed"; fi
    if launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; then
      echo "launchd: registered"
      launchctl print "$DOMAIN/$LABEL" 2>/dev/null | grep -E '^\s+(state|pid) ' | sed 's/^/         /'
    else
      echo "launchd: not registered"
    fi
    pgrep -fl 'Headroom.app' | sed 's/^/running: /' || echo "running: no"
    ;;

  *) echo "usage: $0 install|uninstall|status"; exit 1 ;;
esac
