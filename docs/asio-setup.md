# ASIO Setup

Run `bash scripts/install-asio.sh` as your normal desktop user, not root or sudo.
No arguments always dispatch to the numbered wizard, even with redirected input
or output; they never fall through to noninteractive installation. EOF cancels
safely without building, installing or running Wine. Automation should use
explicit flags. `--interactive` is an alias and must be used alone. The helper can
also be run with `bash scripts/setup-asio.sh`.

The wizard asks for the install root, build directory, whether to build or reuse
artifacts, and whether to register after installing files. Defaults come from
`SIDEALSA_ASIO_INSTALL_ROOT` (otherwise `$HOME/.local`),
`SIDEALSA_ASIO_BUILD_DIR` (otherwise repository `build-asio`), and `WINE`
(otherwise `wine`). Install roots must be absolute. Relative build directories
are resolved against the repository root. Enter paths literally, without shell
quotes or `~` expansion; spaces are supported and shell expressions are not
evaluated. Displayed paths use shell escaping to make unusual characters visible.

## Prefix Discovery

Discovery only reads filesystem metadata. It never runs Wine or creates a
prefix. Registration is split into two steps so Steam games and manual paths
are chosen separately:

1. **Steam games** — `steamapps/compatdata/*/pfx` under the Steam roots below
   (plus extra libraries from each root's `steamapps/libraryfolders.vdf`).
   Each candidate must contain `drive_c/windows/system32`. Games are listed by
   the `name` from their sibling `steamapps/appmanifest_<appid>.acf`, with the
   appid and prefix path shown alongside; entries without a manifest appear as
   `Unknown Steam game`. Select numbers, `all`, Enter to skip, or `0` to cancel.
2. **Manual Wine prefixes** — `$HOME/.wine`, `$WINEPREFIX` if set, plus paths
   entered one per line. Each entry accepts a prefix itself, a Steam library
   root, or a `compatdata` directory, and is validated the same way. Enter an
   empty line to finish, then select numbers, `all`, or Enter to skip.

The Steam roots checked are:

- `$HOME/.steam/steam`
- `$HOME/.steam/root`
- `$HOME/.steam/debian-installation`
- `$HOME/.local/share/Steam`
- `$XDG_DATA_HOME/Steam` when set
- `$HOME/.var/app/com.valvesoftware.Steam/.local/share/Steam`
- `$HOME/.var/app/com.valvesoftware.Steam/data/Steam`
- `$HOME/.var/app/com.valvesoftware.Steam/.steam/steam`
- `$HOME/.var/app/com.valvesoftware.Steam/.steam/root`

Symlink aliases are deduplicated across both steps. A prefix already chosen as a
Steam game is not offered again as a manual entry. Steam library configuration
beyond `libraryfolders.vdf` paths is not parsed; anything else must be entered
explicitly at the manual prompt.

Select one number, multiple space-separated numbers, or `all` per step.
Choosing `all` only selects candidates; it does not register anything yet.
Skipping both steps selects nothing and cancels without invoking the installer.
Invalid selections, `0` at a numbered menu, and EOF cancel without invoking the
installer. Blank path prompts retain defaults; blank additional-path input ends
discovery. Blank numbered choices skip that step.

## Prefix Ownership (Proton / Bottles)

A host Wine process started inside a prefix owned by another Wine build
migrates that prefix to the host build and can damage it. Following
`pipeasio-register`, the wizard and the installer detect ownership from
filesystem metadata only:

- `tracked_files` in the prefix or its parent directory marks a
  Proton-managed prefix.
- `bottle.yml` in the prefix marks a Bottles bottle.

With a default wine lookup, Proton prefixes register through `umu-run`
(`GAMEID=umu-<appid>`) instead of system wine, so the prefix is never migrated
to the host build. Installing `umu` is required for this path. In Bottles
bottles the DLL is staged automatically and `bottles-cli` persists
`WINEDLLPATH` in the bottle environment (Bottles limits the system environment,
so an exported variable would never reach its wine) before running
`regsvr32 /s sidealsa-asio64.dll`. Give the TUI the bottle name for this path;
without a name only the DLL is staged and the printed manual steps apply.
Every registration command is status-checked: a failed `regsvr32` aborts the
install instead of reporting success.

Passing `--wine` or setting `WINE` explicitly counts as accepting
responsibility and bypasses both rules, so only do that with a binary that
owns the prefix. Automatic per-game Proton-build selection is deliberately not
attempted: runners live outside the prefix and cannot be derived reliably from
prefix files alone; `umu-run` manages that choice.

## Confirmation

The final summary separates file installation from prefix registration and lists
the selected Steam games (with appids) and manual prefixes separately. Only an
explicit `y` at the `[y/N]` prompt authorizes execution.
Enter, EOF, or any other response defaults to no, with no build,
installation, or registration. Root execution is rejected before any prompts.

File-only setup never runs Wine. Registration copies the DLL into each selected
prefix and runs Wine `regsvr32`, which **can start Wine processes** and change the
prefix. This requires final confirmation. The wizard does not touch audio
devices, manage services, or restart anything. It delegates to the existing
installer with an argument array; library replacement remains atomic. The full
build/install/register operation is not a transaction: failures after confirmation
may leave completed installations or registrations in place.

Existing noninteractive options remain available through
`bash scripts/install-asio.sh --help`, including `--no-build`, `--no-register`,
and repeated `--steam-prefix PATH`. These explicit noninteractive invocations
retain their existing behavior and do not ask for confirmation.

## Fixture Tests

Run `bash -n scripts/install-asio.sh scripts/setup-asio.sh scripts/test-setup-asio.sh scripts/test-install-asio-prefix-guard.sh`,
`bash scripts/test-setup-asio.sh`, and
`bash scripts/test-install-asio-prefix-guard.sh`. Tests use an isolated
temporary HOME, prefix fixtures and a stub installer (plus mock build artifacts
and logging wine stubs for the guard test); they never invoke real Wine, build
ASIO, install files into the desktop environment, or use live prefixes.
Coverage includes no-argument dispatch with redirected input/output, safe EOF
cancellation, split Steam-game/manual selection with ordering, game-name and
unknown-manifest display, `libraryfolders.vdf` external libraries, alias
deduplication, explicit confirmation reaching only the stub installer,
Proton/Bottles skip-and-report in the wizard, explicit-`WINE` inclusion, and
installer refusal before any Wine process or file copy in foreign prefixes.
