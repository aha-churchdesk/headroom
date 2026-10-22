#!/bin/bash
# No real app, process, signing identity or launch service is changed here.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/install-common.sh
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/headroom-installer-tests.XXXXXX")
trap 'rm -rf "$ROOT"' EXIT
unset HEADROOM_HOME

fail() { echo "FAIL: $*" >&2; exit 1; }
assert_old() { [ "$(cat "$HR_APP/version")" = old ] || fail 'old bundle changed'; }
assert_no_signal() { [ ! -s "$TEST_WORK/signals" ] || fail 'unexpected signal'; }
assert_clean() {
  local leftovers
  leftovers=$(find "$TEST_WORK/apps" -name '.headroom-*' -print)
  [ -z "$leftovers" ] || fail "leftover staging/backup: $leftovers"
}

# Stub external operations; real file copies/renames operate only under ROOT.
ps() { [ ! -f "$TEST_WORK/ps-fail" ] || return 1; cat "$TEST_WORK/processes"; }
kill() {
  [ "$1" = -TERM ] && [ "$2" = 12345 ] || fail "unsafe signal: $*"
  echo "$*" >> "$TEST_WORK/signals"
  touch "$TEST_WORK/stopping"
}
sleep() {
  if [ -f "$TEST_WORK/stopping" ]; then
    local count
    count=$(cat "$TEST_WORK/sleeps")
    count=$((count + 1))
    echo "$count" > "$TEST_WORK/sleeps"
    if [ "$count" -ge "$(cat "$TEST_WORK/exit-after")" ]; then
      : > "$TEST_WORK/processes"
    fi
  fi
}
codesign() {
  echo "$*" >> "$TEST_WORK/signing"
  if [ "$1" = --force ] && [ -f "$TEST_WORK/sign-fail" ]; then return 1; fi
}
plutil() { return 0; }
hr_bundle_executable() { echo headroomd; }
cp() { [ ! -f "$TEST_WORK/copy-fail" ] || return 1; command cp "$@"; }
mv() {
  if [[ "$1" == *'.headroom-stage.'* ]] && [ -f "$TEST_WORK/move-fail" ]; then return 1; fi
  command mv "$@"
}
launchctl() {
  echo "$*" >> "$TEST_WORK/launch"
  case "$1" in
    print) [ -f "$TEST_WORK/registered" ] ;;
    kickstart) [ ! -f "$TEST_WORK/restart-fail" ] ;;
    *) fail "unexpected launchctl: $*" ;;
  esac
}
open() {
  [ "$1" = -a ] && [ "$2" = "$HR_APP" ] || fail "wrong app opened: $*"
  echo "$(cat "$HR_APP/version")" >> "$TEST_WORK/opens"
  if [ -f "$TEST_WORK/restart-fail" ] && [ "$(cat "$HR_APP/version")" = new ]; then return 1; fi
}

fixture() {
  TEST_WORK="$ROOT/$1"
  export TEST_WORK
  HR_APP="$TEST_WORK/apps/Headroom.app"
  HR_PLIST="$TEST_WORK/user/dev.headroom.plist"
  mkdir -p "$HR_APP/Contents/MacOS" "$TEST_WORK/user"
  printf 'old\n' > "$HR_APP/version"
  printf '<plist/>\n' > "$HR_APP/Contents/Info.plist"
  printf '#!/bin/bash\ncat "$TEST_WORK/listing"\n[ ! -f "$TEST_WORK/list-fail" ]\n' > "$HR_APP/Contents/MacOS/headroom"
  printf '#!/bin/bash\nexit 0\n' > "$HR_APP/Contents/MacOS/headroomd"
  chmod +x "$HR_APP/Contents/MacOS/"*
  command cp -R "$HR_APP" "$TEST_WORK/new.app"
  printf 'new\n' > "$TEST_WORK/new.app/version"
  printf 'memory   test\nrunning  0\nqueued   0\n' > "$TEST_WORK/listing"
  printf '%s 12345 %s/Contents/MacOS/headroomd\n' "$(id -u)" "$HR_APP" > "$TEST_WORK/processes"
  echo 0 > "$TEST_WORK/sleeps"
  echo 1 > "$TEST_WORK/exit-after"
  echo 'custom login config' > "$HR_PLIST"
}
expect_failure() {
  if hr_install_bundle "$TEST_WORK/new.app" > "$TEST_WORK/output" 2>&1; then fail 'install unexpectedly succeeded'; fi
}
run_case() (
  fixture "$1"
  "$2"
  echo "ok: $1"
)

busy() {
  printf 'memory   test\nrunning  1\nqueued   0\n' > "$TEST_WORK/listing"
  expect_failure; assert_old; assert_no_signal; assert_clean
}
queued() {
  printf 'memory   test\nrunning  0\nqueued   1 · waiting\n' > "$TEST_WORK/listing"
  expect_failure; assert_old; assert_no_signal; assert_clean
}
unknown() {
  printf 'memory   test\nrunning  0\nqueued   0\nqueued   0\n' > "$TEST_WORK/listing"
  expect_failure; assert_old; assert_no_signal; assert_clean
}
unreachable() {
  touch "$TEST_WORK/list-fail"
  expect_failure; assert_old; assert_no_signal; assert_clean
}
custom_home() {
  export HEADROOM_HOME="$TEST_WORK/other-daemon"
  expect_failure; assert_old; assert_no_signal; assert_clean
}
process_failure() {
  touch "$TEST_WORK/ps-fail"
  expect_failure; assert_old; assert_no_signal; assert_clean
}
near_matches() {
  cat > "$TEST_WORK/processes" <<PS
$(id -u) 11111 /checkout/Headroom.app/Contents/MacOS/headroomd
$(id -u) 11112 $HR_APP/Contents/MacOS/headroomd-helper
$(id -u) 11113 /bin/sh -c $HR_APP/Contents/MacOS/headroomd
99999 11114 $HR_APP/Contents/MacOS/headroomd
PS
  hr_install_bundle "$TEST_WORK/new.app" > "$TEST_WORK/output" 2>&1
  assert_no_signal; assert_clean
  [ "$(cat "$HR_APP/version")" = new ] || fail 'new bundle missing'
}
delayed_exit() {
  echo 15 > "$TEST_WORK/exit-after"
  hr_install_bundle "$TEST_WORK/new.app" > "$TEST_WORK/output" 2>&1
  [ "$(cat "$TEST_WORK/sleeps")" = 15 ] || fail 'did not wait for exit'
  [ "$(cat "$HR_APP/version")" = new ] || fail 'new bundle missing'
  [ "$(cat "$HR_PLIST")" = 'custom login config' ] || fail 'changed login config'
  assert_clean
}
timeout() {
  echo 1000 > "$TEST_WORK/exit-after"
  expect_failure; assert_old; assert_clean
  [ "$(cat "$TEST_WORK/sleeps")" = 100 ] || fail 'wait was not bounded'
  [ "$(cat "$TEST_WORK/signals")" = '-TERM 12345' ] || fail 'unexpected signals'
  [ ! -f "$TEST_WORK/opens" ] || fail 'restarted after timeout'
}
stage_failure() {
  touch "$TEST_WORK/copy-fail"
  expect_failure; assert_old; assert_no_signal; assert_clean
}
sign_failure() {
  touch "$TEST_WORK/sign-fail"
  expect_failure; assert_old; assert_no_signal; assert_clean
}
rollback() {
  touch "$TEST_WORK/move-fail"
  expect_failure; assert_old; assert_clean
  [ "$(cat "$TEST_WORK/opens")" = old ] || fail 'old app not restarted'
}
restart_rollback() {
  touch "$TEST_WORK/restart-fail"
  expect_failure; assert_old; assert_clean
  [ "$(cat "$TEST_WORK/opens")" = $'new\nold' ] || fail 'old app not restarted after launch failure'
}
registered() {
  touch "$TEST_WORK/registered"
  hr_install_bundle "$TEST_WORK/new.app" > "$TEST_WORK/output" 2>&1
  grep -qx "kickstart $HR_SERVICE" "$TEST_WORK/launch" || fail 'service not restarted'
  [ ! -f "$TEST_WORK/opens" ] || fail 'opened app beside registered service'
  [ "$(cat "$HR_PLIST")" = 'custom login config' ] || fail 'changed login config'
  assert_clean
}
relative_bundle() {
  local repo="$TEST_WORK/repo" caller="$TEST_WORK/caller" status
  mkdir -p "$repo/scripts" "$caller/release builds" "$TEST_WORK/stubs"
  caller=$(cd "$caller" && pwd)
  command cp scripts/install.sh "$repo/scripts/install.sh"
  printf 'HR_APP="$TEST_WORK/apps/Headroom.app"\n' > "$repo/scripts/install-common.sh"
  cat > "$repo/scripts/install-bundle.sh" <<'STUB'
#!/bin/bash
printf '%s\n' "$1" > "$TEST_WORK/bundle-argument"
exit 23
STUB
  printf '#!/bin/bash\nexit 1\n' > "$TEST_WORK/stubs/brew"
  chmod +x "$repo/scripts/install-bundle.sh" "$TEST_WORK/stubs/brew"
  # Stop at the install helper, before any app, CLI link or hook is changed.
  if (cd "$caller" && PATH="$TEST_WORK/stubs:$PATH" bash "$repo/scripts/install.sh" \
      --bundle 'release builds/Headroom.app' --bin "$TEST_WORK/bin") > "$TEST_WORK/output" 2>&1; then
    fail 'stub installer unexpectedly succeeded'
  else
    status=$?
  fi
  [ "$status" = 23 ] || fail "installer did not reach stub: $status"
  [ "$(cat "$TEST_WORK/bundle-argument")" = "$caller/release builds/Headroom.app" ] || fail 'bundle path resolved outside caller directory'
  assert_old; assert_no_signal
}
for name in busy queued unknown unreachable custom_home process_failure near_matches delayed_exit timeout stage_failure sign_failure rollback restart_rollback registered relative_bundle; do
  run_case "$name" "$name"
done
