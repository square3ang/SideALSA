#!/usr/bin/env bash
# Update only an existing, main-installer-owned ALSA plugin. No build or services.
set -Eeuo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
SOURCE=${SIDEALSA_ALSA_SOURCE:-$ROOT/target/release/libasound_module_pcm_sidealsa.so}
DEST=${SIDEALSA_ALSA_DESTINATION:-/usr/lib/alsa-lib/libasound_module_pcm_sidealsa.so}
MANIFEST=${SIDEALSA_ALSA_MANIFEST:-/usr/local/share/sidealsa/install-manifest}
BACKUP_PARENT=${SIDEALSA_ALSA_BACKUP_DIR:-$ROOT/target}

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
hash() { sha256sum < "$1" | cut -d ' ' -f 1; }
regular() { [[ -f "$1" && ! -L "$1" ]]; }
# Escalate only the individual operation in a non-writable target directory.
destination_op() {
    local directory=$1
    shift
    if [[ -w "$directory" ]]; then "$@"; else sudo -n -- "$@"; fi
}
matches() { regular "$1" && [[ $(hash "$1") == "$2" ]]; }

(($# == 0)) || die 'configure paths with SIDEALSA_ALSA_{SOURCE,DESTINATION,MANIFEST,BACKUP_DIR}; no arguments'
for path in "$SOURCE" "$DEST" "$MANIFEST" "$BACKUP_PARENT"; do
    [[ "$path" == /* && "$path" != *$'\n'* ]] || die 'paths must be absolute and contain no newlines'
done
[[ "$DEST" != "$MANIFEST" ]] || die 'destination and manifest must differ'
dest_dir=$(dirname -- "$DEST")
manifest_dir=$(dirname -- "$MANIFEST")
for directory in "$dest_dir" "$manifest_dir" "$BACKUP_PARENT"; do
    [[ -d "$directory" ]] || die "directory must already exist: $directory"
done
for path in "$SOURCE" "$DEST" "$MANIFEST"; do
    regular "$path" || die "expected a regular, non-symlink file: $path"
done
source_hash=$(hash "$SOURCE")
old_hash=$(hash "$DEST")
manifest_hash=$(hash "$MANIFEST")
count=0
owned_hash=
while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ "$line" == *$'\t'"$DEST" ]]; then
        owned_hash=${line%$'\t'"$DEST"}
        [[ "$owned_hash" =~ ^[0-9a-f]{64}$ ]] || die 'invalid managed plugin hash'
        count=$((count + 1))
    fi
done < "$MANIFEST"
[[ "$count" == 1 ]] || die 'manifest must contain exactly one matching plugin entry'
[[ "$owned_hash" == "$old_hash" ]] || die 'installed plugin differs from its manifest; refusing unexpected edits'
matches "$MANIFEST" "$manifest_hash" || die 'manifest changed during verification'
if [[ "$source_hash" == "$old_hash" ]]; then
    matches "$SOURCE" "$source_hash" && matches "$DEST" "$old_hash" || die 'files changed during verification'
    printf 'ALSA plugin already verified; no changes\n'
    exit 0
fi

backup=$(mktemp -d "$BACKUP_PARENT/alsa-plugin-backup.XXXXXX")
plugin_stage= manifest_stage= rollback_stage=
swapped=0
cleanup() {
    local status=$?
    trap - EXIT
    if ((swapped)) && ! matches "$DEST" "$old_hash"; then
        if matches "$DEST" "$source_hash" && matches "$MANIFEST" "$manifest_hash" \
            && matches "$rollback_stage" "$old_hash"; then
            if destination_op "$dest_dir" mv -Tf -- "$rollback_stage" "$DEST"; then
                printf 'Restored previous ALSA plugin atomically\n' >&2
            else
                printf 'error: rollback failed; backup: %s\n' "$backup" >&2
            fi
        else
            printf 'error: rollback refused because files changed; backup: %s\n' "$backup" >&2
        fi
    fi
    for path in "$plugin_stage" "$rollback_stage"; do
        [[ -z "$path" ]] || destination_op "$dest_dir" rm -f -- "$path" || true
    done
    [[ -z "$manifest_stage" ]] || destination_op "$manifest_dir" rm -f -- "$manifest_stage" || true
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
cp -- "$DEST" "$backup/plugin.so"
cp -- "$MANIFEST" "$backup/install-manifest"
matches "$backup/plugin.so" "$old_hash" && matches "$backup/install-manifest" "$manifest_hash" \
    || die 'files changed while backing up'

# Preserve every other byte, including a missing final newline.
while :; do
    ending=$'\n'
    IFS= read -r line || { ending=; [[ -n "$line" ]] || break; }
    if [[ "$line" == "$old_hash"$'\t'"$DEST" ]]; then
        line="$source_hash${line:64}"
    fi
    printf '%s%s' "$line" "$ending"
    [[ -n "$ending" ]] || break
done < "$backup/install-manifest" > "$backup/new-manifest"
new_manifest_hash=$(hash "$backup/new-manifest")
plugin_stage=$(destination_op "$dest_dir" mktemp "$dest_dir/.sidealsa-plugin.XXXXXX")
manifest_stage=$(destination_op "$manifest_dir" mktemp "$manifest_dir/.sidealsa-manifest.XXXXXX")
rollback_stage=$(destination_op "$dest_dir" mktemp "$dest_dir/.sidealsa-rollback.XXXXXX")
plugin_mode=$(stat -c '%a' -- "$DEST")
manifest_mode=$(stat -c '%a' -- "$MANIFEST")
destination_op "$dest_dir" install -m "$plugin_mode" -- "$SOURCE" "$plugin_stage"
destination_op "$manifest_dir" install -m "$manifest_mode" -- "$backup/new-manifest" "$manifest_stage"
destination_op "$dest_dir" install -m "$plugin_mode" -- "$backup/plugin.so" "$rollback_stage"
matches "$SOURCE" "$source_hash" && matches "$plugin_stage" "$source_hash" \
    && matches "$rollback_stage" "$old_hash" && matches "$DEST" "$old_hash" \
    && matches "$MANIFEST" "$manifest_hash" && matches "$manifest_stage" "$new_manifest_hash" \
    || die 'files changed before replacement'
# Separate files cannot form one atomic transaction. Recheck before each rename;
# rollback is conditional, never a blind overwrite of a concurrent edit.
swapped=1
destination_op "$dest_dir" mv -Tf -- "$plugin_stage" "$DEST"
matches "$SOURCE" "$source_hash" && matches "$DEST" "$source_hash" \
    && matches "$MANIFEST" "$manifest_hash" && matches "$manifest_stage" "$new_manifest_hash" \
    || die 'files changed before manifest replacement'
destination_op "$manifest_dir" mv -Tf -- "$manifest_stage" "$MANIFEST" \
    || die 'manifest replacement failed'
swapped=0
printf 'Updated ALSA plugin and its manifest entry; backup: %s\n' "$backup"
