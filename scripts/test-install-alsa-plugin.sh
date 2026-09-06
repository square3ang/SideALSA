#!/usr/bin/env bash
# Hardware-free fixtures only. Never invokes the real system installer paths.
set -Eeuo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
temporary=$(mktemp -d "${TMPDIR:-/tmp/opencode}/sidealsa-alsa-install-test.XXXXXX")
trap 'rm -rf -- "$temporary"' EXIT
mkdir -- "$temporary/lib" "$temporary/share" "$temporary/backups"
export SIDEALSA_ALSA_SOURCE="$temporary/source.so"
export SIDEALSA_ALSA_DESTINATION="$temporary/lib/plugin.so"
export SIDEALSA_ALSA_MANIFEST="$temporary/share/install-manifest"
export SIDEALSA_ALSA_BACKUP_DIR="$temporary/backups"
source_file=$SIDEALSA_ALSA_SOURCE
plugin=$SIDEALSA_ALSA_DESTINATION
manifest=$SIDEALSA_ALSA_MANIFEST
hash() { sha256sum < "$1" | cut -d ' ' -f 1; }
run() { bash "$ROOT/scripts/install-alsa-plugin.sh"; }
printf 'old plugin fixture\n' > "$plugin"
printf 'new plugin fixture\n' > "$source_file"
chmod 755 "$plugin"
old_hash=$(hash "$plugin")
new_hash=$(hash "$source_file")
printf '# SideALSA install manifest v1\n  unrelated whitespace\n%s\t%s\nlast entry without newline' \
    "$old_hash" "$plugin" > "$manifest"
cp -- "$manifest" "$temporary/original-manifest"
printf '# SideALSA install manifest v1\n  unrelated whitespace\n%s\t%s\nlast entry without newline' \
    "$new_hash" "$plugin" > "$temporary/expected-manifest"
old_inode=$(stat -c '%i' -- "$plugin")
exec {held}< "$plugin"
run
[[ $(stat -c '%i' -- "$plugin") != "$old_inode" ]]
IFS= read -r held_content <&"$held"
exec {held}<&-
[[ "$held_content" == 'old plugin fixture' ]]
cmp -- "$plugin" "$source_file"
cmp -- "$manifest" "$temporary/expected-manifest"
[[ $(stat -c '%a' -- "$plugin") == 755 ]]
backups=("$temporary"/backups/alsa-plugin-backup.*)
[[ ${#backups[@]} == 1 ]]
[[ $(hash "${backups[0]}/plugin.so") == "$old_hash" ]]
cmp -- "${backups[0]}/install-manifest" "$temporary/original-manifest"
plugin_inode=$(stat -c '%i' -- "$plugin")
manifest_inode=$(stat -c '%i' -- "$manifest")
run
[[ $(stat -c '%i' -- "$plugin") == "$plugin_inode" ]]
[[ $(stat -c '%i' -- "$manifest") == "$manifest_inode" ]]
backups=("$temporary"/backups/alsa-plugin-backup.*)
[[ ${#backups[@]} == 1 ]]
# Even when source equals the edited destination, ownership must be checked.
printf 'unexpected local edit\n' > "$plugin"
cp -- "$plugin" "$source_file"
if run; then printf 'FAIL: accepted an unexpected edit\n' >&2; exit 1; fi
cmp -- "$plugin" "$source_file"
cmp -- "$manifest" "$temporary/expected-manifest"
printf 'new plugin fixture\n' > "$plugin"
printf 'next plugin fixture\n' > "$source_file"
# Inject only a fixture manifest-rename failure, then a concurrent plugin edit.
mkdir -- "$temporary/bin"
export REAL_MV
REAL_MV=$(command -v mv)
printf '%s\n' '#!/usr/bin/env bash' \
    'if [[ "${!#}" == "$SIDEALSA_ALSA_MANIFEST" ]]; then' \
    '    if [[ "${INJECT_EDIT:-0}" == 1 ]]; then' \
    '        printf "concurrent edit\n" > "$SIDEALSA_ALSA_DESTINATION"' \
    '    fi' \
    '    exit 1' \
    'fi' \
    'exec "$REAL_MV" "$@"' > "$temporary/bin/mv"
chmod +x "$temporary/bin/mv"
if PATH="$temporary/bin:$PATH" run; then
    printf 'FAIL: accepted manifest rename failure\n' >&2; exit 1
fi
[[ $(hash "$plugin") == "$new_hash" ]]
cmp -- "$manifest" "$temporary/expected-manifest"
if INJECT_EDIT=1 PATH="$temporary/bin:$PATH" run; then
    printf 'FAIL: accepted concurrent edit\n' >&2; exit 1
fi
IFS= read -r content < "$plugin"
[[ "$content" == 'concurrent edit' ]]
cmp -- "$manifest" "$temporary/expected-manifest"
shopt -s nullglob
stages=("$temporary"/lib/.sidealsa-* "$temporary"/share/.sidealsa-*)
[[ ${#stages[@]} == 0 ]]
printf 'PASS: atomic inode replacement, byte-preserved manifest, backups, verified no-op, ownership refusal, conditional rollback, cleanup\n'
