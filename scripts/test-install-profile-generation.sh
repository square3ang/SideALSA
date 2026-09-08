#!/usr/bin/env bash
# Offline installer tests: real generator, MOCK runtime/plugin artifacts, DESTDIR only.
set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
GENERATOR="$ROOT/target/release/sidealsa-config-gen"
[[ -x "$GENERATOR" ]] || {
    printf 'Build the real generator first: cargo build --release -p sidealsa-config --bin sidealsa-config-gen\n' >&2
    exit 1
}
TMP="$(mktemp -d)"
trap 'rm -rf -- "$TMP"' EXIT
trap 'printf "Fixture failed at line %s\n" "$LINENO" >&2; [[ ! -f "$TMP/install.log" ]] || cat "$TMP/install.log" >&2' ERR
CHECKOUT="$TMP/mock-checkout"
STAGE="$TMP/dest"
mkdir -p "$CHECKOUT/scripts" "$CHECKOUT/target/release" "$CHECKOUT/build-gui" "$CHECKOUT/assets" "$STAGE" "$TMP/guard"
cp "$ROOT/scripts/install.sh" "$CHECKOUT/scripts/"
cp "$ROOT/scripts/uninstall.sh" "$CHECKOUT/scripts/"
cp "$ROOT/assets/sidealsa-icon.png" "$CHECKOUT/assets/"
cp -a "$ROOT/packaging" "$ROOT/configs" "$ROOT/profiles" "$ROOT/docs" "$ROOT/LICENSE" "$CHECKOUT/"
cp "$GENERATOR" "$CHECKOUT/target/release/"
for binary in sidealsa-setup sidealsad sidealsa-hw-test sidealsa-pro-test sidealsa-loopback-test \
    sidealsa-stats sidealsa-pro-client-test sidealsa-shared-test sidealsa-admin; do
    printf '#!/usr/bin/env bash\n# MOCK artifact, never executed by these tests.\nexit 97\n' > "$CHECKOUT/target/release/$binary"
    chmod +x "$CHECKOUT/target/release/$binary"
done
cp "$CHECKOUT/target/release/sidealsa-admin" "$CHECKOUT/build-gui/sidealsa-control"
printf 'MOCK plugin, not loadable\n' > "$CHECKOUT/target/release/libasound_module_pcm_sidealsa.so"
for command in sudo systemctl; do
    printf '#!/usr/bin/env bash\nprintf "forbidden service command\\n" >> "$SERVICE_GUARD_LOG"\nexit 98\n' > "$TMP/guard/$command"
    chmod +x "$TMP/guard/$command"
done
export SERVICE_GUARD_LOG="$TMP/service-commands"
export PATH="$TMP/guard:$PATH"
unset SIDEALSA_SOCKET SIDEALSA_SOCKET_EXPLICIT SUDO_USER SIDEALSA_INSTALL_REEXEC

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
contains() { grep -Fq -- "$2" "$1" || fail "$1 lacks $2"; }
absent() { if grep -Fq -- "$2" "$1"; then fail "$1 unexpectedly contains $2"; fi; }
GUI_ARGS=(--no-gui)
install_fixture() {
    DESTDIR="$STAGE" PREFIX=/usr/local ALSA_PLUGIN_DIR=/usr/lib/alsa-lib \
        bash "$CHECKOUT/scripts/install.sh" --no-build "${GUI_ARGS[@]}" --no-start "$@" > "$TMP/install.log" 2>&1
}
expect_unchanged_failure() {
    local diagnostic=$1
    shift
    rm -rf "$TMP/before"
    cp -a "$STAGE" "$TMP/before"
    if install_fixture "$@"; then fail "unexpected install success: $*"; fi
    contains "$TMP/install.log" "$diagnostic"
    # Archive comparison includes symlinks/FIFOs and modes without opening them.
    cmp <(tar --sort=name -cf - -C "$TMP/before" .) \
        <(tar --sort=name -cf - -C "$STAGE" .) || fail "failed install changed destination"
}
selection() { "$GENERATOR" "$1" "$STAGE/etc/sidealsa/active.toml"; }

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
ALSA="$STAGE/etc/alsa/conf.d/99-sidealsa.conf"
PW_PATH=/etc/pipewire/pipewire.conf.d/99-sidealsa.conf
PW="$STAGE$PW_PATH"
MANIFEST="$STAGE/usr/local/share/sidealsa/install-manifest"
PROFILE="$STAGE/etc/sidealsa/profiles/generic.toml"
install_fixture --profile profiles/generic.toml --socket /tmp/custom.sock || { cat "$TMP/install.log"; fail generic; }
contains "$ALSA" 'pcm.sidealsa_monitors'
absent "$ALSA" 'pcm.sidealsa_line1'
contains "$PW" 'sidealsa-monitors'
contains "$PW" 'api.alsa.headroom    = 128'
contains "$PW" 'api.alsa.start-delay = 256'
contains "$MANIFEST" /usr/local/bin/sidealsa-config-gen
contains "$MANIFEST" /etc/sidealsa/active.toml
[[ "$(selection --selected-profile)" == /etc/sidealsa/profiles/generic.toml ]] || fail selection
[[ "$(selection --selected-socket)" == /tmp/custom.sock ]] || fail socket
contains "$STAGE/etc/systemd/system/sidealsad.service" 'ExecStart=/usr/local/bin/sidealsad --profile /etc/sidealsa/profiles/generic.toml --socket /tmp/custom.sock'
expect_unchanged_failure '--replace-profile requires explicit --profile' --replace-profile --force

# Selection and legacy paths must stay within the managed profiles directory.
ACTIVE="$STAGE/etc/sidealsa/active.toml"
SERVICE="$STAGE/etc/systemd/system/sidealsad.service"
cp "$ACTIVE" "$TMP/trusted-active"
cp "$SERVICE" "$TMP/trusted-service"
for invalid_profile in /etc/other.toml /etc/sidealsa/profiles/../other.toml \
    /etc/sidealsa/profiles/sub/device.toml /etc/sidealsa/profiles/.toml \
    /etc/sidealsa/profiles/./generic.toml; do
    printf 'profile = "%s"\nsocket = "/tmp/custom.sock"\n' "$invalid_profile" > "$ACTIVE"
    expect_unchanged_failure 'direct child' --force
    rm "$ACTIVE"
    printf 'ExecStart=/usr/local/bin/sidealsad --profile %s --socket /tmp/custom.sock\n' "$invalid_profile" > "$SERVICE"
    expect_unchanged_failure 'direct child' --force
    cp "$TMP/trusted-active" "$ACTIVE"
    cp "$TMP/trusted-service" "$SERVICE"
done
cp "$CHECKOUT/profiles/generic.toml" "$CHECKOUT/profiles/.toml"
expect_unchanged_failure 'nonempty .toml stem' --profile profiles/.toml

# Reject both dangling and ordinary symlinks, and nonregular installed files.
for target in "$ACTIVE" "$PROFILE"; do
    mv "$target" "$TMP/link-target"
    for link in "$TMP/link-target" "$TMP/does-not-exist"; do
        ln -s "$link" "$target"
        expect_unchanged_failure 'symlink' --force
        rm "$target"
    done
    mkfifo "$target"
    expect_unchanged_failure 'must be regular' --force
    rm "$target"
    mv "$TMP/link-target" "$target"
done
for directory in "$STAGE/etc/sidealsa/profiles" "$STAGE/etc/sidealsa"; do
    mv "$directory" "$TMP/linked-directory"
    ln -s "$TMP/linked-directory" "$directory"
    expect_unchanged_failure 'symlink' --force
    rm "$directory"
    mv "$TMP/linked-directory" "$directory"
done

# Destination wins even when the explicit seed is different or invalid; force is not replace.
sed -i 's/monitors/retained/g' "$PROFILE"
printf 'malformed seed\n' > "$CHECKOUT/profiles/generic.toml"
install_fixture --profile profiles/generic.toml --force
contains "$ALSA" 'pcm.sidealsa_retained'
contains "$PROFILE" 'id = "retained"'
cp "$PROFILE" "$TMP/valid-profile.toml"
printf 'invalid retained profile\n' > "$PROFILE"
expect_unchanged_failure 'sidealsa-config-gen:' --force --no-pipewire
cp "$TMP/valid-profile.toml" "$PROFILE"
install_fixture
contains "$ALSA" 'pcm.sidealsa_retained'
[[ "$(selection --selected-socket)" == /tmp/custom.sock ]] || fail 'socket replay'
SIDEALSA_SOCKET=/tmp/env.sock install_fixture
[[ "$(selection --selected-socket)" == /tmp/env.sock ]] || fail 'environment socket'
SIDEALSA_SOCKET=/tmp/env.sock install_fixture --socket /tmp/cli.sock
[[ "$(selection --selected-socket)" == /tmp/cli.sock ]] || fail 'CLI socket precedence'

# A malformed replacement must not even retire existing PipeWire files.
expect_unchanged_failure 'sidealsa-config-gen:' --profile profiles/generic.toml --replace-profile --force --no-pipewire
sed 's/retained/replaced/g' "$PROFILE" > "$CHECKOUT/profiles/generic.toml"
install_fixture --profile profiles/generic.toml --replace-profile
contains "$ALSA" 'pcm.sidealsa_replaced'
cmp "$PROFILE" "$CHECKOUT/profiles/generic.toml"

# Preserve all three integration files, including their original manifest hashes.
SNAPSHOT_PATH=/etc/sidealsa/integration-profile.toml
SNAPSHOT="$STAGE$SNAPSHOT_PATH"
cmp "$PROFILE" "$SNAPSHOT"
cp "$SNAPSHOT" "$TMP/generated-snapshot"
grep -F -- "$SNAPSHOT_PATH" "$MANIFEST" > "$TMP/snapshot.hash"
pw_paths=("$PW_PATH" /etc/pipewire/pipewire-pulse.conf.d/99-sidealsa.conf /etc/wireplumber/wireplumber.conf.d/99-sidealsa.conf)
mkdir "$TMP/preserved"
for i in "${!pw_paths[@]}"; do
    path=${pw_paths[$i]}
    printf '\n# locally modified %s\n' "$i" >> "$STAGE$path"
    cp "$STAGE$path" "$TMP/preserved/$i"
    grep -F -- "$path" "$MANIFEST" > "$TMP/preserved/$i.hash"
done
# In-place edits cannot redefine the topology of the already-generated adapters.
sed -i 's/replaced/inplace/g' "$PROFILE"
expect_unchanged_failure 'incompatible profile topology' --preserve-pipewire --force
cp "$TMP/generated-snapshot" "$PROFILE"
sed -i 's/Studio Monitors/Edited Display/' "$PROFILE"
cat >> "$PROFILE" <<'EOF'

[integration.pipewire]
playback_headroom_frames = 256
EOF
install_fixture --preserve-pipewire
cmp "$SNAPSHOT" "$TMP/generated-snapshot"
cp "$TMP/generated-snapshot" "$PROFILE"
printf '\n# tampered snapshot\n' >> "$SNAPSHOT"
expect_unchanged_failure 'snapshot hash mismatch' --preserve-pipewire --force
cp "$TMP/generated-snapshot" "$SNAPSHOT"
mv "$SNAPSHOT" "$TMP/snapshot-link-target"
ln -s "$TMP/snapshot-link-target" "$SNAPSHOT"
expect_unchanged_failure 'symlink' --preserve-pipewire --force
rm "$SNAPSHOT"
mv "$TMP/snapshot-link-target" "$SNAPSHOT"
sed 's/replaced/different/g' "$PROFILE" > "$CHECKOUT/profiles/incompatible.toml"
expect_unchanged_failure 'incompatible profile topology' --profile profiles/incompatible.toml --preserve-pipewire --force
cp "$CHECKOUT/profiles/incompatible.toml" "$CHECKOUT/profiles/generic.toml"
expect_unchanged_failure 'incompatible profile topology' --profile profiles/generic.toml --replace-profile --preserve-pipewire --force
sed -e 's/Studio Monitors/New Display Name/' -e 's/48000/44100/' "$PROFILE" > "$CHECKOUT/profiles/compatible.toml"
cat >> "$CHECKOUT/profiles/compatible.toml" <<'EOF'

[integration.pipewire]
playback_headroom_frames = 512
EOF
install_fixture --profile profiles/compatible.toml --preserve-pipewire
cmp "$SNAPSHOT" "$TMP/generated-snapshot"
grep -F -- "$SNAPSHOT_PATH" "$MANIFEST" > "$TMP/current.hash"
cmp "$TMP/current.hash" "$TMP/snapshot.hash"
for i in "${!pw_paths[@]}"; do
    cmp "$STAGE${pw_paths[$i]}" "$TMP/preserved/$i"
    grep -F -- "${pw_paths[$i]}" "$MANIFEST" > "$TMP/current.hash"
    cmp "$TMP/current.hash" "$TMP/preserved/$i.hash"
done

# Unsafe values must fail even with force, before any destination changes.
for unsafe in '/tmp/a"b' "/tmp/a'b" '/tmp/a\b' '/tmp/a$b' '/tmp/a%b' '/tmp/a b' \
    '/tmp/a`b' '/tmp/a;b' '/tmp/a&b' '/tmp/a(b' '/tmp/a)b' '/tmp/a<b' '/tmp/a>b' '/tmp/a|b' \
    '/tmp/a*b' '/tmp/a?b' '/tmp/a#b' '/tmp/a~b'; do
    expect_unchanged_failure 'unsafe systemd/desktop path' --socket "$unsafe" --force
    expect_unchanged_failure 'unsafe systemd/desktop path' --prefix "$unsafe" --force
    unsafe_profile="$CHECKOUT/profiles/${unsafe##*/}.toml"
    cp "$CHECKOUT/profiles/compatible.toml" "$unsafe_profile"
    expect_unchanged_failure 'unsafe systemd/desktop path' --profile "$unsafe_profile" --force
done

# Known legacy command is replayed; unknown custom commands require an explicit profile.
rm "$STAGE/etc/sidealsa/active.toml"
mv "$SNAPSHOT" "$TMP/legacy-snapshot"
expect_unchanged_failure 'migrate by reinstalling without --preserve-pipewire' --preserve-pipewire --force
mv "$TMP/legacy-snapshot" "$SNAPSHOT"
install_fixture --preserve-pipewire
[[ "$(selection --selected-profile)" == /etc/sidealsa/profiles/compatible.toml ]] || fail legacy
[[ "$(selection --selected-socket)" == /tmp/cli.sock ]] || fail 'legacy socket'
cp "$STAGE/etc/sidealsa/active.toml" "$TMP/valid-active.toml"
printf 'invalid selection\n' > "$STAGE/etc/sidealsa/active.toml"
expect_unchanged_failure 'sidealsa-config-gen:' --force
cp "$TMP/valid-active.toml" "$STAGE/etc/sidealsa/active.toml"
rm "$STAGE/etc/sidealsa/active.toml"
sed -i 's|^ExecStart=.*|ExecStart=/opt/custom-wrapper --custom|' "$STAGE/etc/systemd/system/sidealsad.service"
expect_unchanged_failure 'specify --profile explicitly' --force
rm "$SNAPSHOT"
expect_unchanged_failure 'migrate by reinstalling without --preserve-pipewire' --profile profiles/compatible.toml --preserve-pipewire --force
install_fixture --profile profiles/compatible.toml --force
contains "$PW" 'api.alsa.headroom    = 512'
cmp "$SNAPSHOT" "$STAGE/etc/sidealsa/profiles/compatible.toml"

# More than 64 logical channels is valid for ALSA-only, not PipeWire.
sed -e 's/channels = 2/channels = 65/g' \
    -e "s/channels = \[0, 1\]/channels = [$(seq -s ', ' 0 64)]/" \
    "$CHECKOUT/profiles/compatible.toml" > "$CHECKOUT/profiles/wide.toml"
expect_unchanged_failure 'exceeds 64 logical channels' --profile profiles/wide.toml
sed 's/id = "replaced"/id = "pro"/' "$CHECKOUT/profiles/compatible.toml" > "$CHECKOUT/profiles/reserved.toml"
expect_unchanged_failure "port id 'pro' is reserved" --profile profiles/reserved.toml --no-pipewire
install_fixture --profile profiles/wide.toml --no-pipewire
contains "$ALSA" 'pcm.sidealsa_replaced'
[[ ! -e "$PW" ]] || fail 'PipeWire retirement'
[[ ! -e "$SNAPSHOT" ]] || fail 'snapshot retirement'
absent "$MANIFEST" "$SNAPSHOT_PATH"

# Fresh install retains the reference default; preserve without history refuses.
STAGE="$TMP/fresh"
mkdir "$STAGE"
expect_unchanged_failure 'managed integration profile snapshot' --preserve-pipewire
install_fixture
[[ "$(selection --selected-profile)" == /etc/sidealsa/profiles/topping-e1x2.toml ]] || fail 'fresh default'

# GUI icon ownership, retirement, and uninstall at default and custom prefixes.
GUI_ARGS=()
for prefix in /usr/local /opt/sidealsa; do
    STAGE="$TMP/gui${prefix//\//-}"
    mkdir "$STAGE"
    icon_path="$prefix/share/icons/hicolor/512x512/apps/org.sidealsa.Control.png"
    icon="$STAGE$icon_path"
    MANIFEST="$STAGE$prefix/share/sidealsa/install-manifest"
    install_fixture --prefix "$prefix"
    cmp "$ROOT/assets/sidealsa-icon.png" "$icon"
    [[ $(stat -c '%a' "$icon") == 644 ]] || fail 'icon permissions'
    contains "$STAGE$prefix/share/applications/org.sidealsa.Control.desktop" "Icon=$icon_path"
    read -r icon_hash _ < <(sha256sum "$icon")
    contains "$MANIFEST" "$icon_hash"$'\t'"$icon_path"
    printf 'modified icon\n' >> "$icon"
    expect_unchanged_failure 'managed file changed' --prefix "$prefix"
    expect_unchanged_failure 'retired managed file changed' --prefix "$prefix" --no-gui
    install_fixture --prefix "$prefix" --force
    install_fixture --prefix "$prefix" --no-gui
    [[ ! -e "$icon" ]] || fail 'icon retirement'
    absent "$MANIFEST" "$icon_path"
    install_fixture --prefix "$prefix"
    DESTDIR="$STAGE" bash "$CHECKOUT/scripts/uninstall.sh" --prefix "$prefix" > "$TMP/uninstall.log" 2>&1
    [[ ! -e "$icon" && ! -e "$MANIFEST" ]] || fail 'icon uninstall'
    install_fixture --prefix "$prefix"
    printf 'modified icon\n' >> "$icon"
    DESTDIR="$STAGE" bash "$CHECKOUT/scripts/uninstall.sh" --prefix "$prefix" > "$TMP/uninstall.log" 2>&1
    [[ -f "$icon" ]] || fail 'modified icon not preserved'
    contains "$MANIFEST" "$icon_path"
    DESTDIR="$STAGE" bash "$CHECKOUT/scripts/uninstall.sh" --prefix "$prefix" --force > "$TMP/uninstall.log" 2>&1
    [[ ! -e "$icon" && ! -e "$MANIFEST" ]] || fail 'forced icon uninstall'
done
[[ ! -e "$SERVICE_GUARD_LOG" ]] || fail 'service command invoked'
printf 'PASS: offline installer fixtures (real generator; MOCK runtime/plugin artifacts; no hardware or services)\n'
