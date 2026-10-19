#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-compact.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-compact" ;;
    *) rel="x86_64-apple-darwin/gray-compact" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-compact" ;;
    *) rel="x86_64-unknown-linux-musl/gray-compact" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-compact.exe" "$here/target/release/gray-compact" "$here/target/debug/gray-compact.exe" "$here/target/debug/gray-compact"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-compact binary not found in $here" >&2
exit 1
