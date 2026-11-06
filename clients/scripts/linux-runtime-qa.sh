#!/usr/bin/env bash
# Run as root in a fresh disposable Ubuntu container. Do not preinstall build
# tools or runtime libraries: the package must supply its own dependency closure.
set -euo pipefail
package=${1:?Usage: linux-runtime-qa.sh /path/to/twr.deb [--launch]}
launch=${2:-}
test "$(id -u)" = 0
test -f "$package"
. /etc/os-release
test "$ID" = ubuntu
export DEBIAN_FRONTEND=noninteractive
printf 'Runtime: %s\n' "$PRETTY_NAME"
sha256sum "$package"
printf 'Archive Depends: '
dpkg-deb -f "$package" Depends
apt-get update
# This must be the only install before the dependency inspection below.
apt-get install -y --no-install-recommends "$package"
dpkg-query -W -f='Installed: ${binary:Package} ${Version}\nDepends: ${Depends}\n' twr-desktop
getconf GNU_LIBC_VERSION

count=0
while IFS= read -r -d '' file; do
  magic=$(od -An -tx1 -N4 "$file" | tr -d ' \n')
  if [ "$magic" != 7f454c46 ]; then continue; fi
  count=$((count + 1))
  printf '\nELF: %s\n' "$file"
  if ! output=$(ldd "$file" 2>&1); then
    printf '%s\n' "$output"
    exit 1
  fi
  printf '%s\n' "$output"
  if grep -q 'not found' <<< "$output"; then exit 1; fi
done < <(find /opt/twr -type f -print0)
test "$count" -ge 10
printf '\nPASS: all %s packaged ELF files resolve before GUI test tools are installed.\n' "$count"
git --version

if [ "$launch" != --launch ]; then exit 0; fi
# These tools are deliberately installed only after every ELF passed above.
apt-get install -y --no-install-recommends xvfb xauth dbus-x11 curl procps
useradd --create-home --user-group --shell /bin/bash twr-runtime
fixture=/home/twr-runtime/qa
install -dm700 -o twr-runtime -g twr-runtime "$fixture"
cat > "$fixture/config.toml" <<'EOF'
[orchestrator]
autostart = false
EOF
chown twr-runtime:twr-runtime "$fixture/config.toml"
chmod 600 "$fixture/config.toml"
stat -c 'Sandbox helper: %U:%G %a %n' /opt/twr/chrome-sandbox
test "$(stat -c %u /opt/twr/chrome-sandbox)" = 0

launcher=
cleanup() {
  if [ -n "$launcher" ]; then
    kill -- -"$launcher" 2>/dev/null || true
    wait "$launcher" 2>/dev/null || true
  fi
  cat /tmp/twr-runtime-application.log 2>/dev/null || true
}
trap cleanup EXIT
setsid runuser -u twr-runtime -- env \
  HOME=/home/twr-runtime TWR_HOME="$fixture" \
  TWR_DESKTOP_TEST_AUTOMATION=1 \
  xvfb-run -a dbus-run-session -- \
  /usr/bin/twr-desktop --remote-debugging-port=9223 \
  > /tmp/twr-runtime-application.log 2>&1 &
launcher=$!
ready=false
for ((attempt=0; attempt<120; attempt++)); do
  if curl --silent --fail http://127.0.0.1:9223/json/list > /tmp/twr-runtime-pages.json \
    && grep -q 'file://' /tmp/twr-runtime-pages.json \
    && grep -qE '"type":[[:space:]]*"page"' /tmp/twr-runtime-pages.json; then
    ready=true
    break
  fi
  if ! kill -0 "$launcher" 2>/dev/null; then break; fi
  sleep 0.25
done
test "$ready" = true
cat /tmp/twr-runtime-pages.json
sandboxed=false
for command_file in /proc/[0-9]*/cmdline; do
  command=$(tr '\0' ' ' < "$command_file" 2>/dev/null) || continue
  case "$command" in
    '/opt/twr/twr-desktop --type=renderer '*--enable-sandbox*)
      status=${command_file%cmdline}status
      printf '\nRenderer: %s\n' "$command"
      grep -E '^(Uid|NoNewPrivs|Seccomp|Seccomp_filters):' "$status"
      grep -qE '^NoNewPrivs:[[:space:]]+1$' "$status"
      grep -qE '^Seccomp:[[:space:]]+2$' "$status"
      sandboxed=true
      ;;
  esac
done
test "$sandboxed" = true
printf '\nPASS: installed GUI launched with Chromium sandbox enabled.\n'
