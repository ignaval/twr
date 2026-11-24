#!/bin/sh
# Runs the phone-pairing unit tests (app/src/test/java/tools/twr/mobile/dev/pairing) with plain
# javac and java, no Gradle or Android SDK: the pin, trust manager, proof, routes and racing
# classes are plain Java, and PinnedHttp needs only OkHttp. Offline once the jars are cached;
# `--fetch` downloads the missing ones from Maven Central. Every jar's SHA-256 is checked.
#
#   clients/mobile/android/scripts/test-pinning.sh [--fetch]
#
# Needs a JDK 11 or later (JAVA_HOME, else PATH) with keytool, or the openssl CLI, for the TLS
# tests' certificates. The jars live in $TWR_TEST_JARS (default ~/.cache/twr/android-test-jars).
# CI with the Android toolchain runs the same tests, with the rest of the unit tests, as
# `./gradlew :app:testDebugUnitTest` from clients/mobile/android.
set -eu

android=$(cd "$(dirname "$0")/.." && pwd)
jars=${TWR_TEST_JARS:-${XDG_CACHE_HOME:-$HOME/.cache}/twr/android-test-jars}
if [ -n "${JAVA_HOME:-}" ]; then bin="$JAVA_HOME/bin/"; else bin=""; fi
fetch=no
[ "${1:-}" = "--fetch" ] && fetch=yes

# Maven Central path and SHA-256, the versions app/build.gradle uses.
deps="junit/junit/4.13.2/junit-4.13.2.jar=8e495b634469d64fb8acfa3495a065cbacc8a0fff55ce1e31007be4c16dc57d3
org/hamcrest/hamcrest-core/1.3/hamcrest-core-1.3.jar=66fdef91e9739348df7a096aa384a5685f4e875584cce89386a7a47251c4d8e9
org/bouncycastle/bcprov-jdk18on/1.81/bcprov-jdk18on-1.81.jar=249f396412b0c0ce67f25c8197da757b241b8be3ec4199386c00704a2457459b
com/squareup/okhttp3/okhttp/4.12.0/okhttp-4.12.0.jar=b1050081b14bb7a3a7e55a4d3ef01b5dcfabc453b4573a4fc019767191d5f4e0
com/squareup/okio/okio-jvm/3.6.0/okio-jvm-3.6.0.jar=67543f0736fc422ae927ed0e504b98bc5e269fda0d3500579337cb713da28412
org/jetbrains/kotlin/kotlin-stdlib/1.8.21/kotlin-stdlib-1.8.21.jar=042a1cd1ac976cdcfe5eb63f1d8e0b0b892c9248e15a69c8cfba495d546ea52a"

sha256() { sha256sum "$1" 2>/dev/null | cut -d' ' -f1 || shasum -a 256 "$1" | cut -d' ' -f1; }

classpath=""
missing=0
for dep in $deps; do
    path=${dep%%=*}
    sum=${dep#*=}
    jar="$jars/${path##*/}"
    if [ ! -f "$jar" ] && [ "$fetch" = yes ]; then
        mkdir -p "$jars"
        curl -fsSL -o "$jar.part" "https://repo1.maven.org/maven2/$path"
        if [ "$(sha256 "$jar.part")" != "$sum" ]; then
            rm -f "$jar.part"
            echo "downloaded ${path##*/} doesn't have SHA-256 $sum" >&2
            exit 1
        fi
        mv "$jar.part" "$jar"
    fi
    if [ ! -f "$jar" ]; then
        echo "missing $jar (run with --fetch, or set TWR_TEST_JARS)" >&2
        missing=1
        continue
    fi
    if [ "$(sha256 "$jar")" != "$sum" ]; then
        echo "$jar: SHA-256 isn't $sum" >&2
        exit 1
    fi
    classpath="$classpath:$jar"
done
[ "$missing" = 0 ] || exit 1
classpath=${classpath#:}

out=$(mktemp -d "${TMPDIR:-/tmp}/twr-pinning.XXXXXX")
trap 'rm -rf "$out"' EXIT INT TERM

main="$android/app/src/main/java/tools/twr/mobile/dev"
tests="$android/app/src/test/java/tools/twr/mobile/dev/pairing"
"${bin}javac" -encoding UTF-8 -Xlint:-serial -d "$out" -cp "$classpath" \
    "$main"/pairing/*.java "$main"/PinnedHttp.java "$tests"/*.java
classes=
for file in "$tests"/*Test.java; do
    name=${file##*/}
    classes="$classes tools.twr.mobile.dev.pairing.${name%.java}"
done
# The fixture is found by walking up from the working directory.
cd "$android"
# shellcheck disable=SC2086
"${bin}java" -cp "$out:$classpath" org.junit.runner.JUnitCore $classes
