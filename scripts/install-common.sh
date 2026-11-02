#!/bin/bash
# Shared by the local installer and login-item commands. Source with Bash.
HR_APP="${HR_APP:-/Applications/Headroom.app}"
HR_DOMAIN="gui/$(id -u)"
HR_SERVICE="$HR_DOMAIN/dev.headroom"
HR_PLIST="$HOME/Library/LaunchAgents/dev.headroom.plist"

hr_error() { echo "Headroom: $*" >&2; }

# comm is the executable pathname on macOS, not the command's arguments.
# Match the entire pathname and the current uid; never signal a grep match.
hr_pids() {
  ps -axo uid=,pid=,comm= | awk -v uid="$(id -u)" -v exe="$HR_APP/Contents/MacOS/headroomd" '
    $1 == uid { pid=$2; sub(/^[[:space:]]*[0-9]+[[:space:]]+[0-9]+[[:space:]]+/, ""); if ($0 == exe) print pid }
  '
}

# Every headroomd of this uid, wherever it was started from: a Homebrew copy,
# a build in a checkout. Only one can hold the queue, so an installed app
# started beside another exits at once. Prints "pid path" lines. A name
# that is not an executable on disk (a shell running one, say) is skipped.
hr_any_daemons() {
  local listing pid exe
  listing=$(ps -axo uid=,pid=,comm= | awk -v uid="$(id -u)" '
    $1 == uid { pid=$2; sub(/^[[:space:]]*[0-9]+[[:space:]]+[0-9]+[[:space:]]+/, ""); n=split($0, p, "/"); if (p[n] == "headroomd") print pid, $0 }
  ') || return 1
  while read -r pid exe; do
    [ -n "$pid" ] && [ -f "$exe" ] && [ -x "$exe" ] && echo "$pid $exe"
  done <<< "$listing"
  return 0
}

# The same, for those not started from $HR_APP.
hr_other_daemons() {
  hr_any_daemons | awk -v exe="$HR_APP/Contents/MacOS/headroomd" '{ pid=$1; sub(/^[0-9]+ /, ""); if ($0 != exe) print pid, $0 }'
}

hr_require_idle() {
  local pids listing
  if [ "${HEADROOM_HOME+x}" = x ]; then
    hr_error "unset HEADROOM_HOME before managing the installed app so the queue check uses its default data directory"
    return 1
  fi
  pids=$(hr_any_daemons) || { hr_error "cannot inspect running processes"; return 1; }
  [ -n "$pids" ] || return 0
  # Whichever copy holds the socket answers; ours may not be installed yet.
  listing=$(hr_cli ls) || {
    hr_error "cannot read the live queue; leaving Headroom running"; return 1;
  }
  # Accept only the known empty listing, with an optional held notice. Unknown
  # output, duplicate headers and any running/queued job all fail closed.
  if ! printf '%s\n' "$listing" | awk '
    NR == 1 && /^memory   / { memory=1; next }
    NR == 2 && /^held     new jobs wait until `headroom hold off`$/ { next }
    /^running  0$/ && !running && !queued { running=1; next }
    /^queued   0$/ && running && !queued { queued=1; next }
    { bad=1 }
    END { exit !(memory && running && queued && !bad) }
  '; then
    hr_error "queue is busy or its status is unknown; wait for running and queued jobs to finish"
    return 1
  fi
}

# A CLI that can read the queue: the installed one, else this checkout's.
hr_cli() {
  local cli="$HR_APP/Contents/MacOS/headroom"
  [ -x "$cli" ] || cli="$(dirname "${BASH_SOURCE[0]}")/../target/release/headroom"
  [ -x "$cli" ] || { hr_error "no headroom CLI to read the queue with"; return 1; }
  "$cli" "$@"
}

# Stop copies started from anywhere but $HR_APP, once the queue is idle.
hr_stop_others() {
  local others pid exe n
  others=$(hr_other_daemons) || return 1
  [ -n "$others" ] || return 0
  hr_require_idle || return 1
  while read -r pid exe; do
    echo "stopping Headroom started from $exe (pid $pid); only one copy can run"
    kill -TERM "$pid" 2>/dev/null || true
  done <<< "$others"
  for ((n=0; n<100; n++)); do
    others=$(hr_other_daemons) || return 1
    [ -n "$others" ] || return 0
    sleep 0.1
  done
  hr_error "Headroom from another location did not exit within 10 seconds: $others"
  return 1
}

hr_stop() {
  local pids pid current n
  hr_require_idle || return 1
  hr_stop_others || return 1
  pids=$(hr_pids) || return 1
  for pid in $pids; do
    # Recheck identity immediately before signalling, including the uid.
    current=$(hr_pids) || return 1
    if printf '%s\n' "$current" | grep -qx "$pid"; then
      kill -TERM "$pid" || {
        current=$(hr_pids) || return 1
        if printf '%s\n' "$current" | grep -qx "$pid"; then
          hr_error "could not stop process $pid"; return 1
        fi
      }
    fi
  done
  # Ten seconds, without escalation to SIGKILL. Also catches service respawns.
  for ((n=0; n<100; n++)); do
    current=$(hr_pids) || return 1
    [ -n "$current" ] || return 0
    sleep 0.1
  done
  hr_error "Headroom did not exit within 10 seconds; keeping the installed bundle"
  return 1
}

hr_wait_started() {
  local deadline=$((SECONDS + 10)) pids
  # A successful launch command does not mean macOS executed the app. Require
  # its exact process and a live socket response. Jobs may already be arriving.
  # The CLI's five-second response timeout can extend the deadline by that much.
  while [ "$SECONDS" -lt "$deadline" ]; do
    pids=$(hr_pids) || return 1
    if [ -n "$pids" ] && "$HR_APP/Contents/MacOS/headroom" ls >/dev/null 2>&1; then
      pids=$(hr_pids) || return 1
      [ -z "$pids" ] || return 0
    fi
    sleep 0.1
  done
  hr_error "the installed daemon did not respond after launch"
  local others
  others=$(hr_other_daemons)
  if [ -n "$others" ]; then
    hr_error "another copy holds the queue, so the installed one exited: $others"
  elif launchctl print "$HR_SERVICE" >/dev/null 2>&1; then
    hr_error "login item: $(launchctl print "$HR_SERVICE" | awk -F' = ' '/^\tprogram|last exit code/ { printf "%s=%s ", $1, $2 }')"
  fi
  return 1
}

hr_restart() {
  if launchctl print "$HR_SERVICE" >/dev/null 2>&1; then
    if launchctl kickstart "$HR_SERVICE" && hr_wait_started; then return 0; fi
    hr_error "registered service did not start; trying the installed app directly"
  fi
  open -a "$HR_APP" || return 1
  hr_wait_started
}

hr_validate_bundle() {
  local app="$1"
  [ -d "$app" ] && [ ! -L "$app" ] &&
    [ -x "$app/Contents/MacOS/headroom" ] &&
    [ -x "$app/Contents/MacOS/headroomd" ] &&
    [ -f "$app/Contents/Info.plist" ] || {
      hr_error "incomplete app bundle: $app"; return 1;
    }
  plutil -lint "$app/Contents/Info.plist" >/dev/null || return 1
  [ "$(hr_bundle_executable "$app/Contents/Info.plist")" = headroomd ] || return 1
  codesign --verify --deep --strict "$app" || return 1
}

hr_bundle_executable() {
  /usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$1"
}

hr_install_bundle() (
  # Stage and retain the old bundle on the same filesystem as the destination.
  # The EXIT trap restores the old pathname if replacement or restart fails.
  local source="$1" stage="" backup="" stopped=0 installed=0 committed=0
  hr_cleanup() {
    local status=$?
    trap - EXIT HUP INT TERM
    if [ "$committed" -eq 0 ]; then
      if [ "$installed" -eq 1 ]; then
        # The replacement may have started accepting jobs even if its health
        # check failed. Never remove it under a busy or unresponsive daemon.
        if ! hr_stop; then
          hr_error "cannot safely roll back; keeping $HR_APP and the previous app at $backup/Headroom.app"
          [ -z "$stage" ] || rm -rf "$stage"
          return 1
        fi
        rm -rf "$HR_APP"
      fi
      if [ -n "$backup" ] && [ -e "$backup/Headroom.app" ]; then
        if ! mv "$backup/Headroom.app" "$HR_APP"; then
          hr_error "restore failed; previous app is at $backup/Headroom.app"
          return 1
        fi
      fi
      if [ "$stopped" -eq 1 ] && [ -d "$HR_APP" ]; then
        hr_restart || hr_error "could not restart the previous app; open $HR_APP manually"
      fi
    fi
    [ -z "$stage" ] || rm -rf "$stage"
    [ -z "$backup" ] || rmdir "$backup" 2>/dev/null || true
    return "$status"
  }
  trap hr_cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' HUP TERM
  [ ! -L "$HR_APP" ] || { hr_error "refusing to replace a symlink: $HR_APP"; exit 1; }
  hr_validate_bundle "$source" || exit 1
  stage=$(mktemp -d "$(dirname "$HR_APP")/.headroom-stage.XXXXXX") || exit 1
  cp -R "$source" "$stage/Headroom.app" || exit 1
  # Sign and verify the entire staged copy before stopping the installed app.
  codesign --force --sign - "$stage/Headroom.app/Contents/MacOS/headroom" || exit 1
  codesign --force --sign - "$stage/Headroom.app" || exit 1
  hr_validate_bundle "$stage/Headroom.app" || exit 1
  backup=$(mktemp -d "$(dirname "$HR_APP")/.headroom-backup.XXXXXX") || exit 1
  hr_stop || exit 1
  stopped=1
  if [ -e "$HR_APP" ]; then mv "$HR_APP" "$backup/Headroom.app" || exit 1; fi
  installed=1
  mv "$stage/Headroom.app" "$HR_APP" || exit 1
  hr_restart || exit 1
  committed=1
  rm -rf "$backup"
  backup=""
  echo "installed and started $HR_APP"
)
