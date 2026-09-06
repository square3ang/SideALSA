#!/usr/bin/env bash
# Hardware-free atomic installer test with text fixtures, never Wine modules.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
temporary=$(mktemp -d "${TMPDIR:-/tmp}/sidealsa-asio-install-test.XXXXXX")
trap 'rm -rf -- "$temporary"' EXIT
build="$temporary/build"
prefix="$temporary/install"
mkdir -p -- "$build"
printf 'unchanged PE fixture\n' > "$build/sidealsa-asio64.dll"
printf 'old Unix fixture\n' > "$build/sidealsa-asio64.dll.so"
bash "$ROOT/scripts/install-asio.sh" --no-build --no-register --build-dir "$build" --install-root "$prefix"
unix="$prefix/lib/wine/x86_64-unix/sidealsa-asio64.dll.so"
pe="$prefix/lib/wine/x86_64-windows/sidealsa-asio64.dll"
old_inode=$(stat -c '%i' -- "$unix")
pe_inode=$(stat -c '%i' -- "$pe")
exec {held}< "$unix"
printf 'new Unix fixture\n' > "$build/sidealsa-asio64.dll.so"
bash "$ROOT/scripts/install-asio.sh" --no-build --no-register --build-dir "$build" --install-root "$prefix"
[[ "$(stat -c '%i' -- "$unix")" != "$old_inode" ]]
[[ "$(stat -c '%i' -- "$pe")" == "$pe_inode" ]]
IFS= read -r held_content <&"$held"
exec {held}<&-
[[ "$held_content" == 'old Unix fixture' ]]
IFS= read -r new_content < "$unix"
[[ "$new_content" == 'new Unix fixture' ]]
[[ "$(stat -c '%a' -- "$unix")" == 755 ]]
[[ "$(readlink -- "$prefix/lib/wine/x86_64-unix/sidealsa-asio.dll.so")" == sidealsa-asio64.dll.so ]]
printf 'PASS: atomic replacement preserves open old files and unchanged PE files\n'
