#!/usr/bin/env bash
# Offline update.sh tests: staged DESTDIR only, mock artifacts, no services.
set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
GENERATOR="$ROOT/target/release/sidealsa-config-gen"
[[ -x "$GENERATOR" ]] || {
    printf 'Build the real generator first: cargo build --release -p sidealsa-config --bin sidealsa-config-gen\n' >&2
    exit 1
}
TMP="$(mktemp -d)"
trap 'rm -rf -- "$TMP"' EXIT
CHECKOUT="$TMP/mock-checkout"
STAGE="$TMP/dest"
mkdir -p "$CHECKOUT/scripts" "$CHECKOUT/target/release" "$CHECKOUT/build-gui" \
    "$CHECKOUT/build-asio" "$CHECKOUT/assets" "$STAGE" "$TMP/guard" "$TMP/bin"
cp "$ROOT/scripts/install.sh" "$ROOT/scripts/update.sh" "$CHECKOUT/scripts/"
cp "$ROOT/assets/sidealsa-icon.png" "$CHECKOUT/assets/"
cp -a "$ROOT/packaging" "$ROOT/configs" "$ROOT/profiles" "$ROOT/docs" "$ROOT/LICENSE" "$CHECKOUT/"
cp "$GENERATOR" "$CHECKOUT/target/release/"
for binary in sidealsa-setup sidealsa-reconnect sidealsad sidealsa-hw-test sidealsa-pro-test sidealsa-loopback-test \
    sidealsa-stats sidealsa-pro-client-test sidealsa-shared-test sidealsa-admin; do
    printf '#!/usr/bin/env bash\n# MOCK artifact, never executed by these tests.\nexit 97\n' > "$CHECKOUT/target/release/$binary"
    chmod +x "$CHECKOUT/target/release/$binary"
done
cp "$CHECKOUT/target/release/sidealsa-admin" "$CHECKOUT/build-gui/sidealsa-control"
printf 'MOCK plugin, not loadable\n' > "$CHECKOUT/target/release/libasound_module_pcm_sidealsa.so"
printf 'MOCK ASIO PE\n' > "$CHECKOUT/build-asio/sidealsa-asio64.dll"
printf 'MOCK ASIO Unix\n' > "$CHECKOUT/build-asio/sidealsa-asio64.dll.so"
# setup.sh must never run: any dispatch is a failure.
printf '%s\n' '#!/usr/bin/env bash' 'echo "setup dispatched: $*" >> "$SETUP_GUARD_LOG"' 'exit 99' \
    > "$CHECKOUT/scripts/setup.sh"
chmod +x "$CHECKOUT/scripts/setup.sh"
for command in sudo systemctl; do
    printf '#!/usr/bin/env bash\nprintf "forbidden service command\\n" >> "$SERVICE_GUARD_LOG"\nexit 98\n' > "$TMP/guard/$command"
    chmod +x "$TMP/guard/$command"
done
# Presence-only Wine toolchain stubs: --no-build never executes them.
for command in winegcc winebuild; do
    printf '#!/usr/bin/env bash\nexit 0\n' > "$TMP/bin/$command"
    chmod +x "$TMP/bin/$command"
done
export SERVICE_GUARD_LOG="$TMP/service-commands"
export SETUP_GUARD_LOG="$TMP/setup-commands"
export PATH="$TMP/bin:$TMP/guard:$PATH"
unset SIDEALSA_SOCKET SIDEALSA_SOCKET_EXPLICIT SUDO_USER SIDEALSA_INSTALL_REEXEC SIDEALSA_UPDATE

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
contains() { grep -Fq -- "$2" "$1" || fail "$1 lacks $2"; }
absent() { if grep -Fq -- "$2" "$1"; then fail "$1 unexpectedly contains $2"; fi; }

cat > "$CHECKOUT/profiles/generic.toml" <<'EOF'
[device]
name = "Generic Fixture"
rate = 48000
period_size = 64
buffer_size = 256
[device.playback]
device = "hw:Fixture,0"
channels = 2
format = "S32_LE"
[device.capture]
device = "hw:Fixture,0"
channels = 2
format = "S32_LE"
[[ports.playback]]
id = "monitors"
name = "Studio Monitors"
channels = [0, 1]
[[ports.capture]]
id = "talkback"
name = "Talkback"
channels = [0]
EOF

export DESTDIR="$STAGE" PREFIX=/usr/local ALSA_PLUGIN_DIR=/usr/lib/alsa-lib
PROFILE="$STAGE/etc/sidealsa/profiles/generic.toml"
MANIFEST="$STAGE/usr/local/share/sidealsa/install-manifest"

# update.sh must never reference the setup menus.
absent <(cat "$CHECKOUT/scripts/update.sh") 'setup.sh'

rm -f -- "$TMP/setup-commands" "$TMP/service-commands"
bash "$CHECKOUT/scripts/install.sh" --no-build --no-gui --no-start \
    --profile profiles/generic.toml --socket /tmp/custom.sock > "$TMP/install.log" 2>&1
[[ ! -e "$TMP/setup-commands" && ! -e "$TMP/service-commands" ]] || fail 'seed install escaped staging'
[[ -f "$PROFILE" ]] || fail 'seed profile missing'
sha_before=$(sha256sum -- "$PROFILE")
absent "$MANIFEST" 'sidealsa-asio64.dll'
absent "$MANIFEST" 'bin/sidealsa-control'

# Bare update: no TUI, profile preserved, features unchanged.
rm -f -- "$TMP/setup-commands" "$TMP/service-commands"
bash "$CHECKOUT/scripts/update.sh" --no-build --no-start > "$TMP/update.log" 2>&1
[[ ! -e "$TMP/setup-commands" && ! -e "$TMP/service-commands" ]] || fail 'update escaped staging'
contains "$TMP/update.log" 'preserving existing profile'
[[ "$(sha256sum -- "$PROFILE")" == "$sha_before" ]] || fail 'update changed profile'
absent "$MANIFEST" 'sidealsa-asio64.dll'
absent "$MANIFEST" 'bin/sidealsa-control'

# Enable ASIO through install.sh, then update must follow it automatically.
bash "$CHECKOUT/scripts/install.sh" --no-build --no-gui --no-start --with-asio \
    > "$TMP/asio.log" 2>&1
contains "$MANIFEST" 'sidealsa-asio64.dll'
sha_before=$(sha256sum -- "$PROFILE")
rm -f -- "$TMP/setup-commands" "$TMP/service-commands"
bash "$CHECKOUT/scripts/update.sh" --no-build --no-start > "$TMP/update2.log" 2>&1
[[ ! -e "$TMP/setup-commands" && ! -e "$TMP/service-commands" ]] || fail 'asio update escaped staging'
contains "$TMP/update2.log" 'preserving existing profile'
contains "$MANIFEST" 'sidealsa-asio64.dll'
[[ -f "$STAGE/usr/local/lib/wine/x86_64-windows/sidealsa-asio64.dll" ]] || fail 'asio dll retired by update'
[[ "$(sha256sum -- "$PROFILE")" == "$sha_before" ]] || fail 'asio update changed profile'

# Profile-changing and unknown options are refused without touching staging.
for refused in --profile --replace-profile --interactive --bogus; do
    rm -rf "$TMP/before"
    cp -a "$STAGE" "$TMP/before"
    if bash "$CHECKOUT/scripts/update.sh" "$refused" profiles/generic.toml > "$TMP/refused.log" 2>&1; then
        fail "update accepted $refused"
    fi
    cmp <(tar --sort=name -cf - -C "$TMP/before" .) \
        <(tar --sort=name -cf - -C "$STAGE" .) || fail "refused $refused changed destination"
done

bash "$CHECKOUT/scripts/update.sh" --help > "$TMP/help.log" 2>&1
contains "$TMP/help.log" 'preserving'

printf 'Update fixture tests passed (staged only, no services or TUI).\n'
