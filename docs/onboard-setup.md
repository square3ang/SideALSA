# Onboard Setup

Run as a normal user from a source checkout:

```sh
bash scripts/setup.sh
bash scripts/install.sh
bash scripts/install.sh --interactive
# Or build and invoke directly:
cargo build --release -p sidealsa-cli --bin sidealsa-setup
target/release/sidealsa-setup --project-root /absolute/path/to/SideALSA
```

No-argument `install.sh` always dispatches to setup, even with redirected input
or output. No-argument `setup.sh` requires terminal stdin and stdout and fails
before Cargo otherwise. Automation must use explicit installer flags; it never
falls through to a default installation. `--interactive` remains an alias used alone.

The wrapper builds as the current user, supplies the checkout root as a separate
argument, and never re-executes as root. The installer alone may request sudo.
The binary requires a terminal for interactive use. Numbered menus accept Enter
for the displayed default, `b` to return to the main menu, and `c` to cancel.
EOF cancels too. Returning back after saving keeps the saved draft and returns
to the main menu.
The direct binary uses the current directory unless `--project-root` is supplied;
installation menus require a checkout, not a build-machine path embedded in the binary.

## Discovery

Playback and capture are listed separately using alsa 0.11 `card::Iter`, `Ctl`,
`DeviceIter`, and `Ctl::pcm_info`. This opens ALSA **control** devices, not audio
PCMs. No `PCM::new`, capability probe, stream start, mixer write, or audio transfer
is performed. Discovery can be incomplete due to permissions or unavailable
control metadata; manual explicit `hw:CARD=id,DEV=n` selectors remain available.
Manual setup selects each direction independently. Supported-device selection
uses USB VID:PID from the ALSA card's sysfs USB ancestry, not evdev identifiers:

| USB VID:PID | Profile | Evidence |
| --- | --- | --- |
| `152a:8755` | `topping-e1x2.toml`, E1x2 OTG | Verified locally |
| `152a:8756` | `topping-e2x2.toml`, E2x2 OTG | Source-backed, provisional; not locally hardware verified |

The E2x2 OTG ID is backed by
[tpmix's USB device table](https://github.com/square3ang/tpmix/blob/c6feeebc3563156b25822b1e3c1be2087f7b6f06/tpmix.cpp#L2995-L3002).
`152a:8752` (non-OTG) is not automatically selected. Missing or unknown USB
identity does not trigger a name-based vendor match.

Selecting a supported device automatically binds its matching vendor profile to
the actual ALSA card's DEV0 playback and capture, preserving vendor channel routing
and timing. Both directions must be listed on that card. It proposes a unique
local draft without channel-count or path questions; multiple supported cards
require an explicit choice. This selects one interface, not a combined device.

```sh
target/release/sidealsa-setup --help
target/release/sidealsa-setup --list-devices
# The wrapper also accepts explicit help/list without a terminal:
bash scripts/setup.sh --help
bash scripts/setup.sh --list-devices
```

Help is offline, with no enumeration. Explicit listing works without a TTY and
has no configuration/service side effects, but does open control devices.

## Defaults

`profiles/generic-onboard-analog.toml` is an unverified template, not a measured
hardware preset. Replace its two card placeholders before use. The wizard renders
confirmed channel counts (default two each) into full-channel logical `output`
and `input` ports. Proposed defaults are S32_LE, 48000 Hz, logical period 64,
physical period 64, hardware buffer 256, shared buffer 512, `duplex_link=false`,
and `pro_latency_periods=1`. Generic RT priorities are hardware 50 and PRO 48;
desktop integration uses priority 10. Actual IRQ ordering still needs checking;
these are proposals, not measured scheduler guarantees. There is no startup
loopback calibration or device-specific tuning.

Channel counts, formats, rates, and geometry are **not capability verified**,
including for USB-matched profiles. Capability checks are future work, not
implemented discovery behavior.
`Profile` parsing/validation checks software consistency only. Hardware may
reject these settings. There is no multi-device aggregation (including analog+HDMI) and no synchronization
of independent clocks; separate cards may drift even though unlinked settings
pass validation. Prefer playback/capture belonging to the same physical clock.

## Saving And Installing

The wizard shows parsed names and paths for checkout and installed profiles.
It displays `selection::installed_selection()` or legacy service `ExecStart`
text read-only, without evaluating it or migrating configuration just to inspect.

Manual setup and existing-profile selection default to **SAVE ONLY**.
Supported USB selection defaults to **install and restart**, but this
is only a proposed action: each `[y/N]` confirmation defaults to no, so only an
explicit `y` proceeds. Supported drafts use a unique local filename
automatically. For manual setup,
the default new path is
`<project-root>/profiles/onboard-local.toml`. Parents must exist. Drafts are
validated with `Profile` and written using `create_new`; existing files and
symlinks are never overwritten. Choose another path instead. Existing profile
selection leaves its source unchanged. Answer `y` to commit the choice.
Cancellation/EOF before this point writes nothing and invokes no installer.

Optional install choices call the existing `scripts/install.sh` using Rust
`Command` arguments, never shell evaluation. The draft is saved first, then GUI
and ASIO menus explain the installer semantics: omitting ASIO removes previously
managed ASIO files; omitting GUI removes managed GUI/helper files. Inclusion may
require Qt/Wine build dependencies. Both inclusion menus default to include so
existing optional components are not silently retired.

The command uses `--profile PATH --replace-profile`: it deliberately replaces
the installed same-basename profile and updates `/etc/sidealsa/active.toml`.
It also regenerates ALSA/PipeWire integration and system service files. Existing
installer safety checks still apply; the wizard does not pass `--force`.
Environment settings such as `PREFIX` and `DESTDIR` retain installer semantics.
The wizard rejects root-like `DESTDIR` values such as `/`; they are live-system
destinations, not safe staging directories. A valid staging destination is shown
as having no service effects. Known template placeholders cannot be installed.

Install/select without immediate start passes `--no-start`. **This enables the
service for future boots**; it does not stop an already-running daemon. Answer
`y` after the final summary to proceed. Install+restart opens hardware, can
interrupt audio and stop/restore user PipeWire, and enables future boots.
Cancelling either leaves the saved draft.
No installer is invoked without an explicit `y` confirmation.

## Offline Tests

```sh
bash scripts/test-setup.sh
cargo test -p sidealsa-cli --bin sidealsa-setup
cargo clippy -p sidealsa-cli --bin sidealsa-setup -- -D warnings
```

Tests use fake enumeration entries and temporary profile files, not hardware or
live installation/services. Hardware capability and sustained streaming tests
must be performed separately and deliberately after reviewing the profile.
