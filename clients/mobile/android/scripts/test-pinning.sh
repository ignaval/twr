#!/bin/sh
# Runs the phone-pairing unit tests (app/src/test/java/tools/twr/mobile/dev/pairing) with plain
# javac and java, no Gradle or Android SDK: the pin, trust manager, proof, routes and racing
# classes are plain Java. Offline once the three jars are cached; `--fetch` downloads them from
# Maven Central and checks their SHA-256.
#
#   clients/mobile/android/scripts/test-pinning.sh [--fetch]
#
# Needs a JDK 11 or later (JAVA_HOME, else PATH) with keytool, or the openssl CLI, for the
# TLS test's certificate. CI with the Android toolchain runs the same tests as
# `./gradlew :app:testDebugUnitTest` from clients/mobile/android.
set -eu

android=$(cd "$(dirname "$0")/.." && pwd)
jars=${TWR_TEST_JARS:-${XDG_CACHE_HOME:-$HOME/.cache}/twr/android-test-jars}
if [ -n "${JAVA_HOME:-}" ]; then bin="$JAVA_HOME/bin/"; else bin=""; fi

# name, Maven Central path, SHA-256
deps="junit-4.13.2.jar junit/junit/4.13.2/junit-4.13.2.jar 8e495b634469d64fb8acfa3495a065cbacc8a0fff55ce1e31007be4c16dc57d3
hamcrest-core-1.3.jar org/hamcrest/hamcrest-core/1.3/hamcrest-core-1.3.jar 66fdef91e9739348df7a096aa384a5685f4e875584cce89386a7a47251c4d8e9
bcprov-jdk18on-1.81.jar org/bouncycastle/bcprov-jdk18on/1.81/bcprov-jdk18on-1.81.jar 249f396412b0c0ce67f25c8197da757b241b8be3ec4199386c00704a2457459b"

if [ "${1:-}" = "--fetch" ]; then
    mkdir -p "$jars"
    echo "$deps" | while read -r name path sum; do
        [ -f "$jars/$name" ] && continue
        curl -fsSL -o "$jars/$name.part" "https://repo1.maven.org/maven2/$path"
        mv "$jars/$name.part" "$jars/$name"
    done
fi

classpath=""
missing=0
echo "$deps" > "${TMPDIR:-/tmp}/twr-pinning-deps.$$"
while read -r name path sum; do
    if [ ! -f "$jars/$name" ]; then
        echo "missing $jars/$name (run with --fetch, or set TWR_TEST_JARS)" >&2
        missing=1
        continue
    fi
    actual=$(sha256sum "$jars/$name" | cut -d' ' -f1)
    if [ "$actual" != "$sum" ]; then
        echo "$jars/$name: SHA-256 $actual, expected $sum" >&2
        exit 1
    fi
    classpath="$classpath:$jars/$name"
done < "${TMPDIR:-/tmp}/twr-pinning-deps.$$"
rm -f "${TMPDIR:-/tmp}/twr-pinning-deps.$$"
[ "$missing" = 0 ] || exit 1
classpath=${classpath#:}

out=$(mktemp -d "${TMPDIR:-/tmp}/twr-pinning.XXXXXX")
trap 'rm -rf "$out"' EXIT INT TERM

main="$android/app/src/main/java/tools/twr/mobile/dev/pairing"
tests="$android/app/src/test/java/tools/twr/mobile/dev/pairing"
"${bin}javac" -encoding UTF-8 -Xlint:-serial -d "$out" -cp "$classpath" "$main"/*.java "$tests"/*.java
classes=$(cd "$tests" && ls *Test.java | sed 's/\.java$//; s/^/tools.twr.mobile.dev.pairing./')
# The fixture is found by walking up from the working directory.
cd "$android"
# shellcheck disable=SC2086
"${bin}java" -cp "$out:$classpath" org.junit.runner.JUnitCore $classes
