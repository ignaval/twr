#!/bin/sh
# Captures the runner protocol fixtures of one revision of twr, the evidence that this hub can
# drain a runner of that protocol (twr_core::runner::DRAINABLE_PROTOCOLS):
#
#   docs/tools/capture-runner-fixtures.sh <rev> <protocol>
#   docs/tools/capture-runner-fixtures.sh --worktree <protocol>
#
# <rev> is a commit whose runner speaks <protocol>: the last one before the bump that left it
# behind, or the commit that bumped it to the current protocol. --worktree captures this
# checkout's working tree as it is (its tracked files, changes not committed included; files
# git does not track yet are left out): the change that bumps the protocol, whose own
# fixtures the fixture test needs before that change can be committed. Its fixtures name HEAD
# as their revision, with "uncommitted": true when the tree had changes. The script makes a temporary
# git worktree of <rev>, adds docs/tools/capture-runner-fixtures.rs to its twr-core as a
# test, builds and runs it there (that revision's own types, its own toolchain and lock file),
# and writes what it captured to crates/twr-core/tests/fixtures/runner-protocol-<protocol>/:
#
#   runner.json        what a runner of <rev> sends (each message, each event kind)
#   hub-accepts.json   what this checkout's hub sends a drained runner, as <rev> decoded it
#   auth-vectors.json  the handshake proofs <rev> computes, and its sealed channel
#
# It fails when <rev> does not speak <protocol>, does not decode one of this checkout's
# values exactly, or has an event kind the capture program does not classify. Commit what it
# writes; crates/twr-core/tests/runner_protocol_fixtures.rs checks this checkout against it.
# A runner.json or auth-vectors.json of a protocol already released should not change: look
# at `git diff` before you commit. hub-accepts.json changes whenever an outbound value does.
#
# Environment:
#   TWR_CAPTURE_DIR   where the temporary worktree and its build go (default:
#                     target/capture-runner-fixtures in this checkout); removed at the end.
#                     Put it on a disk with room for a build, not on a tmpfs.
#   CARGO_BUILD_JOBS  as for cargo (the build is one cargo job at a time)

set -eu

if [ $# -ne 2 ]; then
    echo "usage: docs/tools/capture-runner-fixtures.sh <rev> <protocol>" >&2
    echo "       docs/tools/capture-runner-fixtures.sh --worktree <protocol>" >&2
    exit 2
fi
rev=$1
protocol=$2
case "$protocol" in
    '' | *[!0-9]*)
        echo "capture-runner-fixtures: the protocol must be a number, not $protocol" >&2
        exit 2
        ;;
esac

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
if [ "$rev" = --worktree ]; then
    # A commit of the working tree's tracked files, made without touching the branch, the
    # index or the stash list (none: nothing changed since HEAD).
    commit=$(git stash create) || exit 1
    # (The fixtures name HEAD, and whether there were changes on top: that commit is not
    # kept.)
    label=$(git rev-parse --verify HEAD)
    if [ -n "$commit" ]; then
        uncommitted=1
        described="$label and the working tree's changes"
    else
        commit=$label
        uncommitted=0
        described=$label
    fi
    if [ -n "$(git ls-files --others --exclude-standard -- crates)" ]; then
        echo "capture-runner-fixtures: warning: files under crates/ that git does not track \
are left out (git add them first)" >&2
    fi
else
    commit=$(git rev-parse --verify --quiet "$rev^{commit}") || {
        echo "capture-runner-fixtures: $rev is no commit here" >&2
        exit 1
    }
    label=$commit
    uncommitted=0
    described=$label
fi
spoken=$(git show "$commit:crates/twr-core/src/runner.rs" |
    sed -n 's/^pub const PROTOCOL_VERSION: u32 = \([0-9]*\);$/\1/p')
if [ "$spoken" != "$protocol" ]; then
    echo "capture-runner-fixtures: $rev speaks runner protocol ${spoken:-?}, not $protocol" >&2
    exit 1
fi

scratch=${TWR_CAPTURE_DIR:-$root/target/capture-runner-fixtures}
case "$scratch" in
    /*) ;;
    *) scratch="$(pwd)/$scratch" ;;
esac
if [ -e "$scratch" ]; then
    echo "capture-runner-fixtures: $scratch exists; remove it (and \`git worktree prune\`) first" >&2
    exit 1
fi
cleanup() {
    if [ -d "$scratch/src" ]; then
        git -C "$root" worktree remove --force "$scratch/src" || true
    fi
    rm -rf "$scratch"
    git -C "$root" worktree prune || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$scratch/out"
echo "Printing this checkout's outbound values..."
cargo run --locked -q -p twr-core --example runner_outbound_samples < /dev/null \
    > "$scratch/outbound.json"

echo "Checking out $commit..."
git worktree add --quiet --detach "$scratch/src" "$commit"
mkdir -p "$scratch/src/crates/twr-core/tests"
cp docs/tools/capture-runner-fixtures.rs \
    "$scratch/src/crates/twr-core/tests/capture_runner_fixtures.rs"

echo "Building and running the capture at $commit (protocol $protocol)..."
(
    cd "$scratch/src"
    TWR_CAPTURE_PROTOCOL="$protocol" TWR_CAPTURE_REV="$label" \
        TWR_CAPTURE_UNCOMMITTED="$uncommitted" TWR_CAPTURE_OUTBOUND="$scratch/outbound.json" TWR_CAPTURE_OUT="$scratch/out" \
        CARGO_TARGET_DIR="$scratch/target" CARGO_INCREMENTAL=0 \
        cargo test --locked -q -p twr-core --test capture_runner_fixtures -- --exact capture \
        < /dev/null
)

dest="crates/twr-core/tests/fixtures/runner-protocol-$protocol"
mkdir -p "$dest"
for f in runner.json hub-accepts.json auth-vectors.json; do
    cp "$scratch/out/$f" "$dest/$f"
done
echo "Wrote $dest (from $described); check \`git diff -- $dest\` before committing."
