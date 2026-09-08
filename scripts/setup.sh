#!/usr/bin/env bash
set -Eeuo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
if (($# == 0)) && [[ ! -t 0 || ! -t 1 ]]; then
    printf '%s\n' 'Interactive setup requires terminal stdin and stdout; use --help or --list-devices, or explicit installer flags for automation.' >&2
    exit 1
fi
if (( EUID == 0 )); then
    printf '%s\n' 'Run setup as your normal user, not root or sudo.' >&2
    exit 1
fi
# Do not accept a caller-supplied root; all forwarded arguments stay separate.
for arg in "$@"; do
    if [[ "$arg" == --project-root || "$arg" == --project-root=* ]]; then
        printf '%s\n' 'setup.sh supplies --project-root itself.' >&2
        exit 1
    fi
done
cargo build --manifest-path "$ROOT/Cargo.toml" --release -p sidealsa-cli --bin sidealsa-setup --target-dir "$ROOT/target"
exec "$ROOT/target/release/sidealsa-setup" --project-root "$ROOT" "$@"
