#!/bin/sh
# Builds twr from this checkout and installs it for the current user, to try changes before
# they are released:
#
#   docs/tools/dev-install.sh
#
# It builds the release profile of twr-cli and installs the binary the way install.sh does:
# staged next to the target, run once, then renamed over the old one (a running twr keeps
# its binary). `twr --version` then names the commit (e.g. 0.2.0+dev.69988b4, ".dirty" with
# uncommitted changes).
# A running hub keeps its old binary until you restart it (`twr hub stop`, then `twr`). It
# writes a checkpoint of each workspace's log for the new build first (`twr log checkpoint
# --all`), so that the restart does not fold the whole log.
# The install one-liner goes back to the latest release, and so does `twr update`.
#
# Environment:
#   TWR_INSTALL_DIR   where to put `twr` (default: ~/.local/bin)

set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
dir=${TWR_INSTALL_DIR:-$HOME/.local/bin}
case "$dir" in
    /*) ;;
    *) dir="$(pwd)/$dir" ;; # relative to where it was run, not the checkout
esac
staged=""
trap '[ -z "$staged" ] || rm -f "$staged"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

cd "$root"
if ! command -v cargo > /dev/null 2>&1; then
    # rustup puts cargo on PATH through ~/.cargo/env, which a non-login sh may not have read.
    if [ -f "${CARGO_HOME:-$HOME/.cargo}/env" ]; then
        # shellcheck source=/dev/null
        . "${CARGO_HOME:-$HOME/.cargo}/env"
    fi
    command -v cargo > /dev/null 2>&1 || {
        echo "dev-install: cargo not found; install Rust with rustup (https://rustup.rs)" >&2
        exit 1
    }
fi
build=$(git rev-parse --short HEAD)
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    build="$build.dirty"
fi

echo "Building twr ($build)..."
TWR_BUILD="$build" CARGO_INCREMENTAL=0 cargo build --release --locked -p twr-cli

mkdir -p "$dir"
if [ -d "$dir/twr" ]; then
    echo "dev-install: $dir/twr is a directory" >&2
    exit 1
fi
staged=$(mktemp "$dir/.twr.new.XXXXXX")
cp target/release/twr "$staged"
chmod 755 "$staged"
installed=$("$staged" --version)
mv -f "$staged" "$dir/twr"
staged=""
echo "Installed $installed to $dir/twr"

case ":${PATH:-}:" in
    *":$dir:"*) ;;
    *) echo "$dir is not on your PATH" ;;
esac
# Each workspace's log gets a checkpoint for this build (best effort; one up to date is left
# alone): the next start of its hub folds only what comes after it, not the whole log.
# Checkpoints are kept per build, so a running hub of another build keeps its own.
echo ""
echo "Writing a checkpoint of each workspace's log for this build..."
"$dir/twr" log checkpoint --all || echo "dev-install: twr log checkpoint failed (the next hub start folds the whole log)"
if running=$("$dir/twr" info 2> /dev/null | head -1) && [ -n "$running" ]; then
    case "$running" in
        *"$build"*) ;;
        *)
            echo ""
            echo "A hub is running an older build: $running"
            echo "Restart it to use this one: twr hub stop, then twr"
            echo "For all workspaces in this home: twr hub stop --all, then reopen the workspaces you need"
            ;;
    esac
fi
