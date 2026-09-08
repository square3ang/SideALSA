#!/usr/bin/env bash
# Shell dispatch only: stub helpers/Cargo, no builds, enumeration or installation.
set -Eeuo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
((EUID != 0)) || { printf 'Run fixture tests as a non-root user.\n' >&2; exit 1; }
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
mkdir -p "$tmp/repo/scripts" "$tmp/repo/target/release" "$tmp/bin"
cp -- "$ROOT/scripts/install.sh" "$tmp/repo/scripts/install.sh"
printf '%s\n' '#!/usr/bin/env bash' 'printf "setup:%s\n" "$*" > "$CALL_LOG"' > "$tmp/repo/scripts/setup.sh"
printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\n" "$0" >> "$FORBIDDEN_LOG"' 'exit 99' > "$tmp/bin/forbidden"
chmod +x "$tmp/bin/forbidden"
for command in cargo sudo systemctl wine cmake install; do ln -s forbidden "$tmp/bin/$command"; done
export PATH="$tmp/bin:$PATH" CALL_LOG="$tmp/calls" FORBIDDEN_LOG="$tmp/forbidden"
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
for args in '' --interactive; do
    rm -f -- "$CALL_LOG"
    argv=()
    [[ -z "$args" ]] || argv+=("$args")
    bash "$tmp/repo/scripts/install.sh" "${argv[@]}" < /dev/null > "$tmp/output" 2>&1
    [[ $(< "$CALL_LOG") == setup: ]] || fail 'redirected invocation did not dispatch to setup without arguments'
done
rm -f -- "$CALL_LOG"
bash "$tmp/repo/scripts/install.sh" --help < /dev/null > "$tmp/output"
[[ ! -e "$CALL_LOG" ]] || fail 'installer help dispatched to setup'
if bash "$tmp/repo/scripts/install.sh" --interactive --no-build < /dev/null > "$tmp/output" 2>&1; then
    fail 'mixed interactive flags accepted'
fi

# Restore the real wrapper: no-argument non-TTY must fail before Cargo.
cp -- "$ROOT/scripts/setup.sh" "$tmp/repo/scripts/setup.sh"
for script in setup.sh install.sh; do
    if bash "$tmp/repo/scripts/$script" < /dev/null > "$tmp/output" 2>&1; then
        fail "$script accepted non-TTY setup"
    fi
    grep -Fq 'requires terminal stdin and stdout' "$tmp/output" || fail 'missing terminal diagnostic'
done
[[ ! -e "$FORBIDDEN_LOG" ]] || fail 'build/install/service command executed'

# Explicit offline help/list still reach the binary, with arguments intact.
rm -- "$tmp/bin/cargo"
printf '%s\n' '#!/usr/bin/env bash' 'exit 0' > "$tmp/bin/cargo"
printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\0" "$@" > "$CALL_LOG"' > "$tmp/repo/target/release/sidealsa-setup"
chmod +x "$tmp/bin/cargo" "$tmp/repo/target/release/sidealsa-setup"
for option in --help --list-devices; do
    bash "$tmp/repo/scripts/setup.sh" "$option" < /dev/null > "$tmp/output"
    mapfile -d '' -t args < "$CALL_LOG"
    [[ ${#args[@]} == 3 && ${args[0]} == --project-root && ${args[1]} == "$tmp/repo" && ${args[2]} == "$option" ]] || fail 'explicit help/list not forwarded'
done
[[ ! -e "$FORBIDDEN_LOG" ]] || fail 'forbidden command executed'
printf 'Main setup fixture tests passed (no real builds, installers, enumeration or services).\n'
