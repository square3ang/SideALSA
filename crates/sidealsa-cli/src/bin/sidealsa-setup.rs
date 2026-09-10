//! Control-only discovery and a deliberately non-probing setup menu.
use alsa::{Ctl, Direction, card, ctl::DeviceIter};
use sidealsa_config::{Profile, selection};
use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    process::Command,
};
use toml_edit::{Array, DocumentMut, value};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const TEMPLATE: &str = include_str!("../../../../profiles/generic-onboard-analog.toml");
/// Staging area for generated drafts, relative to the checkout root. One
/// constant so the generator, the replace guard, the profile scan, and the
/// tests cannot drift apart. The installer derives the `/etc` filename from
/// the staging basename, so this name is part of the deployed contract.
const STAGING_DIR: &str = "profiles/local";
const RULE: &str = "──────────────────────────────────────────────────────────────";

/// ANSI styling is terminal-only: unit tests capture a pipe, and
/// `NO_COLOR`/`CLICOLOR=0`/dumb terminals also disable it. Wrapping keeps the
/// inner text contiguous, so substring assertions stay valid either way.
fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
        && std::env::var_os("CLICOLOR").is_none_or(|v| v != "0")
        && std::env::var_os("TERM").is_none_or(|v| v != "dumb")
        && io::stdout().is_terminal()
}

fn paint(code: &str, text: &str) -> String {
    if use_color() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn bold(text: &str) -> String {
    paint("1", text)
}

fn dim(text: &str) -> String {
    paint("2", text)
}

fn green(text: &str) -> String {
    paint("32", text)
}

fn section(out: &mut impl Write, title: &str) -> io::Result<()> {
    writeln!(out)?;
    writeln!(out, "{}", bold(title))?;
    writeln!(out, "{}", dim(RULE))?;
    Ok(())
}

fn row(out: &mut impl Write, key: &str, value: std::fmt::Arguments<'_>) -> io::Result<()> {
    writeln!(out, "  {key:<10} {value}")
}

/// ALSA PCM name (`S32_LE`), not the Rust variant Debug spelling (`S32Le`).
fn sample_format(format: &sidealsa_config::SampleFormat) -> &'static str {
    match format {
        sidealsa_config::SampleFormat::S32Le => "S32_LE",
    }
}

#[derive(Debug)]
struct Entry {
    card: i32,
    device: i32,
    usb: Option<(u16, u16)>,
    direction: Direction,
    selector: String,
    name: String,
}

fn usb_identity(device: &Path) -> Option<(u16, u16)> {
    let device = device.canonicalize().ok()?;
    for ancestor in device.ancestors() {
        let vendor = ancestor.join("idVendor");
        let product = ancestor.join("idProduct");
        // Never skip a broken nearer identity and match a parent USB hub.
        if vendor.symlink_metadata().is_ok() || product.symlink_metadata().is_ok() {
            let parse = |path: &Path| {
                let text = fs::read_to_string(path).ok()?;
                let text = text.trim();
                (text.len() == 4 && text.bytes().all(|b| b.is_ascii_hexdigit()))
                    .then(|| u16::from_str_radix(text, 16).ok())
                    .flatten()
            };
            return Some((parse(&vendor)?, parse(&product)?));
        }
    }
    None
}

fn vendor_profile(usb: Option<(u16, u16)>) -> Option<(&'static str, &'static str)> {
    match usb? {
        (0x152a, 0x8755) => Some(("topping-e1x2.toml", "E1x2 OTG; verified locally")),
        (0x152a, 0x8756) => Some(("topping-e2x2.toml", "E2x2 OTG; source-backed, UNVERIFIED")),
        _ => None,
    }
}

fn supported(entries: &[Entry]) -> Vec<&Entry> {
    entries
        .iter()
        .filter(|e| {
            e.device == 0
                && e.direction == Direction::Playback
                && vendor_profile(e.usb).is_some()
                && entries.iter().any(|capture| {
                    capture.card == e.card
                        && capture.device == 0
                        && capture.direction == Direction::Capture
                        && capture.usb == e.usb
                })
        })
        .collect()
}

fn bind_vendor(text: &str, selector: &str) -> Result<String> {
    let mut doc: DocumentMut = text.parse()?;
    for direction in ["playback", "capture"] {
        doc["device"][direction]["device"] = value(selector);
    }
    let text = doc.to_string();
    Profile::from_toml(&text)?;
    Ok(text)
}

/// Stable staging name: one file per card, replaced on every run. Repeated
/// installs therefore reuse the same `/etc` file instead of piling up
/// numbered copies. The name keeps the card so two identical devices stay
/// separate. Only files under `profiles/local/` may use replace semantics.
fn local_path(root: &Path, profile: &str, card: i32) -> PathBuf {
    root.join(STAGING_DIR).join(format!(
        "{}-card{card}-local.toml",
        profile.trim_end_matches(".toml")
    ))
}

fn discover() -> Vec<Entry> {
    let mut entries = Vec::new();
    for card in card::Iter::new() {
        let result = (|| -> Result<()> {
            let card = card?;
            let ctl = Ctl::from_card(&card, true)?;
            let info = ctl.card_info()?;
            let id = info.get_id()?;
            let index = card.get_index();
            let usb = usb_identity(&PathBuf::from(format!(
                "/sys/class/sound/card{index}/device"
            )));
            for device in DeviceIter::new(&ctl) {
                for direction in [Direction::Playback, Direction::Capture] {
                    match ctl.pcm_info(device as u32, 0, direction) {
                        Ok(pcm) => entries.push(Entry {
                            card: index,
                            device,
                            usb,
                            direction,
                            selector: format!("hw:CARD={id},DEV={device}"),
                            name: format!("{} / {}", info.get_name()?, pcm.get_name()?),
                        }),
                        Err(e) if e.errno() == libc::ENOENT || e.errno() == libc::ENXIO => {}
                        Err(e) => eprintln!(
                            "Control metadata unavailable for {id}:{device} {direction:?}: {e}"
                        ),
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("Control enumeration incomplete: {e}. Manual selection is available.");
        }
    }
    entries
}

#[derive(Debug)]
enum Navigation {
    Back,
    Cancel,
}
impl std::fmt::Display for Navigation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl Error for Navigation {}

fn ask(
    input: &mut impl BufRead,
    out: &mut impl Write,
    label: &str,
    default: &str,
) -> Result<String> {
    if default.is_empty() {
        write!(out, "{label} {}: ", dim("(required; b: back, c: cancel)"))?;
    } else {
        write!(
            out,
            "{label} [{default}] {}: ",
            dim("(Enter: default; b: back, c: cancel)")
        )?;
    }
    out.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Err(Box::new(Navigation::Cancel));
    }
    match line.trim() {
        "b" => Err(Box::new(Navigation::Back)),
        "c" => Err(Box::new(Navigation::Cancel)),
        "" => Ok(default.into()),
        answer => Ok(answer.into()),
    }
}

/// Yes-or-no confirmation defaulting to no. Only an explicit `y` proceeds;
/// `b` steps back, while EOF, Enter, and any other answer cancel.
fn confirm(input: &mut impl BufRead, out: &mut impl Write, label: &str) -> Result<()> {
    write!(out, "{label} [y/N] (y: yes, Enter: no, b: back): ")?;
    out.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Err(Box::new(Navigation::Cancel));
    }
    match line.trim().to_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        "b" => Err(Box::new(Navigation::Back)),
        _ => Err(Box::new(Navigation::Cancel)),
    }
}
fn ask_explicit(input: &mut impl BufRead, out: &mut impl Write) -> Result<String> {
    // No default: an empty line re-prompts instead of silently accepting.
    // EOF/`b`/`c` keep their navigation meaning.
    loop {
        let answer = ask(input, out, "Choice (number, no default)", "")?;
        if answer.is_empty() {
            writeln!(out, "Type a listed number; nothing is selected by default.")?;
            continue;
        }
        return Ok(answer);
    }
}

fn menu(
    input: &mut impl BufRead,
    out: &mut impl Write,
    title: &str,
    items: &[String],
    default: usize,
) -> Result<usize> {
    writeln!(out)?;
    writeln!(out, "{}", bold(title))?;
    let width = items.len().to_string().len();
    for (i, item) in items.iter().enumerate() {
        let marker = if default == i + 1 {
            format!(" {}", green("← default"))
        } else {
            String::new()
        };
        writeln!(out, "  {:>width$}. {item}{marker}", i + 1)?;
    }
    loop {
        let answer = if (1..=items.len()).contains(&default) {
            ask(input, out, "Choice", &default.to_string())?
        } else {
            ask_explicit(input, out)?
        };
        if let Ok(n) = answer.parse::<usize>()
            && (1..=items.len()).contains(&n)
        {
            return Ok(n);
        }
        writeln!(out, "Enter a number from 1 to {}.", items.len())?;
    }
}

fn selector(
    input: &mut impl BufRead,
    out: &mut impl Write,
    entries: &[Entry],
    direction: Direction,
) -> Result<String> {
    let available: Vec<_> = entries
        .iter()
        .filter(|e| e.direction == direction)
        .collect();
    let width = available
        .iter()
        .map(|e| e.selector.len())
        .max()
        .unwrap_or(0);
    let mut items = vec!["Enter explicit physical hw: selector manually".into()];
    items.extend(
        available
            .iter()
            .map(|e| format!("{:width$}  {}", e.selector, e.name, width = width)),
    );
    let choice = menu(
        input,
        out,
        &format!("{direction:?} PCMs (control metadata only)"),
        &items,
        1,
    )?;
    if choice > 1 {
        return Ok(available[choice - 2].selector.clone());
    }
    loop {
        let s = ask(input, out, "Explicit hw:CARD=id,DEV=n (no default)", "")?;
        if s.starts_with("hw:") && s.contains(',') && !s.chars().any(char::is_control) {
            return Ok(s);
        }
        writeln!(
            out,
            "Specify a physical hw: card and device, not default/plughw."
        )?;
    }
}

fn channels(input: &mut impl BufRead, out: &mut impl Write, direction: &str) -> Result<u32> {
    loop {
        let s = ask(
            input,
            out,
            &format!("Confirm proposed {direction} channels, NOT capability verified"),
            "2",
        )?;
        if let Ok(n) = s.parse::<u32>()
            && (1..=64).contains(&n)
        {
            return Ok(n);
        }
        writeln!(
            out,
            "Enter 1..64; Profile validation may impose tighter limits."
        )?;
    }
}

fn generate(
    playback: &str,
    capture: &str,
    playback_channels: u32,
    capture_channels: u32,
) -> Result<String> {
    if !(1..=64).contains(&playback_channels) || !(1..=64).contains(&capture_channels) {
        return Err("channel count outside 1..64".into());
    }
    let mut doc: DocumentMut = TEMPLATE.parse()?;
    for (direction, selector, count) in [
        ("playback", playback, playback_channels),
        ("capture", capture, capture_channels),
    ] {
        doc["device"][direction]["device"] = value(selector);
        doc["device"][direction]["channels"] = value(i64::from(count));
        let mapping: Array = (0..count).map(i64::from).collect();
        doc["ports"][direction]
            .as_array_of_tables_mut()
            .ok_or("missing ports")?
            .get_mut(0)
            .ok_or("missing port")?["channels"] = value(mapping);
    }
    let text = doc.to_string();
    Profile::from_toml(&text)?;
    Ok(text)
}

struct Draft {
    path: PathBuf,
    text: String,
    create: bool,
    action: usize,
    /// Only our `profiles/local/` staging files may be replaced. Anything the
    /// user typed keeps never-overwrite protection.
    replace: bool,
}

fn plan(
    input: &mut impl BufRead,
    out: &mut impl Write,
    root: &Path,
    entries: &[Entry],
    existing: &[(PathBuf, String)],
) -> Result<Draft> {
    let supported = supported(entries);
    writeln!(out, "\n{}", bold("SideALSA profile setup"))?;
    writeln!(
        out,
        "{}",
        dim("Control metadata only — nothing is probed, saved, or installed until you confirm.")
    )?;
    if supported.is_empty() {
        writeln!(
            out,
            "{}",
            dim("No supported USB device detected; manual options follow.")
        )?;
    } else {
        writeln!(
            out,
            "{}",
            green(&format!(
                "{} supported USB device(s) detected.",
                supported.len()
            )),
        )?;
    }
    let mut items: Vec<String> = supported
        .iter()
        .map(|e| {
            let (profile, label) = vendor_profile(e.usb).unwrap();
            let (vid, pid) = e.usb.unwrap();
            format!(
                "[supported] {}  —  {} (card{}, DEV0, USB {vid:04x}:{pid:04x})  →  profiles/{profile} [{label}]",
                e.name, e.selector, e.card
            )
        })
        .collect();
    items.extend([
        "[manual] Create onboard draft".into(),
        "[manual] Select an existing profile (no automatic selection)".into(),
    ]);
    let choice = menu(
        input,
        out,
        "Step 1 — Device / profile",
        &items,
        if supported.len() > 1 { 0 } else { 1 },
    )?;
    let is_supported = choice <= supported.len();
    let (path, text, create, replace) = if is_supported {
        let entry = supported[choice - 1];
        let (profile, _) = vendor_profile(entry.usb).unwrap();
        let text = bind_vendor(
            &fs::read_to_string(root.join("profiles").join(profile))?,
            &entry.selector,
        )?;
        let path = local_path(root, profile, entry.card);
        writeln!(
            out,
            "\nAuto-selected {} for {}.",
            bold(&format!("profiles/{profile}")),
            bold(&entry.selector),
        )?;
        writeln!(out, "Draft path: {}", path.display())?;
        writeln!(
            out,
            "{}",
            dim(
                "Vendor routing, channel counts, and timing are preserved; only the card address is bound. No channel or path questions follow. The same staging file is reused on every run."
            )
        )?;
        (path, text, true, true)
    } else if choice == supported.len() + 1 {
        let playback = selector(input, out, entries, Direction::Playback)?;
        let capture = selector(input, out, entries, Direction::Capture)?;
        let pc = channels(input, out, "playback")?;
        let cc = channels(input, out, "capture")?;
        let text = generate(&playback, &capture, pc, cc)?;
        let default = root.join(STAGING_DIR).join("onboard-local.toml");
        let path = loop {
            let s = ask(
                input,
                out,
                "New draft path (existing parent required; never overwrite)",
                &default.to_string_lossy(),
            )?;
            let path = PathBuf::from(s);
            let path = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            if fs::symlink_metadata(&path).is_ok() {
                writeln!(
                    out,
                    "Path already exists. Choose another path, back or cancel."
                )?;
            } else {
                break path;
            }
        };
        (path, text, true, false)
    } else {
        if existing.is_empty() {
            writeln!(out, "No readable valid profiles found.")?;
            return Err(Box::new(Navigation::Back));
        }
        let items = existing
            .iter()
            .map(|(p, name)| format!("{name:?} ({p:?})"))
            .collect::<Vec<_>>();
        // No default profile, especially not the first vendor profile.
        let i = menu(
            input,
            out,
            "Existing profiles: explicitly choose a number",
            &items,
            0,
        )? - 1;
        let path = existing[i].0.clone();
        let text = fs::read_to_string(&path)?;
        (path, text, false, false)
    };
    summary(out, &path, &text)?;
    let action = menu(input, out, "Step 2 — Final action", &[
        "SAVE ONLY (existing profile: leave unchanged); no installation or service effects".into(),
        "Install/select with --no-start: ENABLE service for FUTURE BOOTS, no immediate start/restart".into(),
        "Install/select and RESTART hardware service now; enable future boots".into(),
    ], if is_supported { 3 } else { 1 })?;
    section(out, "Review")?;
    row(out, "Source", format_args!("{}", path.display()))?;
    row(
        out,
        "Installer",
        format_args!("{}", root.join("scripts/install.sh").display()),
    )?;
    if action == 1 {
        row(
            out,
            "Effect",
            format_args!("installed selection unchanged; no services touched"),
        )?;
    } else {
        row(
            out,
            "Select",
            format_args!(
                "/etc/sidealsa/active.toml -> /etc/sidealsa/profiles/{} (replace)",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
        )?;
        row(
            out,
            "Rewrite",
            format_args!("system ALSA/PipeWire integration and service files; may request sudo"),
        )?;
        row(
            out,
            "Service",
            format_args!(
                "{}",
                if action == 2 {
                    "--no-start enables future boots; an already-running service is not stopped or restarted"
                } else {
                    "RESTART opens hardware and may interrupt audio; PipeWire services remain running"
                }
            ),
        )?;
    }
    confirm(input, out, "Commit this choice")?;
    Ok(Draft {
        path,
        text,
        create,
        action,
        replace,
    })
}

fn summary(out: &mut impl Write, path: &Path, text: &str) -> Result<()> {
    let p = Profile::from_toml(text)?;
    let d = p.device;
    section(out, "Profile summary")?;
    row(out, "Name", format_args!("{:?}", d.name))?;
    row(out, "File", format_args!("{}", path.display()))?;
    row(
        out,
        "Playback",
        format_args!(
            "{}  ·  {} ch  ·  {}",
            d.playback.device,
            d.playback.channels,
            sample_format(&d.playback.format)
        ),
    )?;
    row(
        out,
        "Capture",
        format_args!(
            "{}  ·  {} ch  ·  {}",
            d.capture.device,
            d.capture.channels,
            sample_format(&d.capture.format)
        ),
    )?;
    row(out, "Rate", format_args!("{} Hz", d.rate))?;
    row(
        out,
        "Period",
        format_args!(
            "logical {}  ·  physical {}",
            d.period_size,
            d.effective_hardware_period_size()
        ),
    )?;
    row(
        out,
        "Buffer",
        format_args!(
            "hardware {}  ·  shared {}",
            d.buffer_size,
            d.effective_shared_buffer_size()
        ),
    )?;
    row(
        out,
        "Duplex",
        format_args!(
            "link {}  ·  PRO latency {} period(s)",
            d.effective_duplex_link(),
            d.pro_latency_periods
        ),
    )?;
    row(
        out,
        "RT prio",
        format_args!(
            "hardware {}  ·  PRO {}",
            d.realtime_priority,
            d.effective_pro_realtime_priority()
        ),
    )?;
    for (direction, ports) in [("play", &p.ports.playback), ("cap", &p.ports.capture)] {
        for port in ports {
            writeln!(
                out,
                "  {direction:<10} {} {:?}: ch {:?}",
                port.id, port.name, port.channels
            )?;
        }
    }
    writeln!(out, "{}", dim(RULE))?;
    writeln!(
        out,
        "{}",
        dim(
            "Notes: control metadata only, geometry NOT verified · no analog+HDMI aggregation · independent clocks are NOT synchronized."
        )
    )?;
    Ok(())
}

fn save_new(path: &Path, text: &str) -> Result<()> {
    Profile::from_toml(text)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// Replace-write for our own `profiles/local/` staging files only. The path
/// must stay inside the staging directory so user files and shipped profiles
/// keep never-overwrite protection.
fn save_replace(root: &Path, path: &Path, text: &str) -> Result<()> {
    Profile::from_toml(text)?;
    let staging = root.join(STAGING_DIR);
    let canonical = path
        .parent()
        .ok_or("staging file needs a parent directory")?
        .canonicalize()?;
    if canonical != staging.canonicalize()? {
        return Err("replace is only allowed inside profiles/local".into());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn install(
    input: &mut impl BufRead,
    out: &mut impl Write,
    root: &Path,
    draft: &Draft,
) -> Result<()> {
    let staged = staged_destination(std::env::var_os("DESTDIR").as_deref())?;
    let selected = Profile::from_toml(&draft.text)?;
    if selected.device.playback.device.contains("YOUR_")
        || selected.device.capture.device.contains("YOUR_")
    {
        return Err("template placeholders cannot be installed; create an onboard draft with selected devices first".into());
    }
    section(out, "Optional components")?;
    writeln!(
        out,
        "{}",
        dim("Draft is saved (or existing source unchanged). Cancelling now leaves it in place.")
    )?;
    writeln!(
        out,
        "{}",
        dim(
            "WARNING: omitting ASIO removes previously managed ASIO binaries; --no-gui removes managed GUI/helper files. Inclusion may require build dependencies."
        )
    )?;
    let gui = menu(
        input,
        out,
        "GUI components",
        &[
            "Include/build GUI and helper".into(),
            "Omit GUI; REMOVE existing managed GUI/helper".into(),
        ],
        1,
    )? == 1;
    let asio = menu(
        input,
        out,
        "Wine ASIO components",
        &[
            "Include/build ASIO (Wine build tools required)".into(),
            "Omit ASIO; REMOVE existing managed ASIO".into(),
        ],
        1,
    )? == 1;
    let mut command = Command::new(root.join("scripts/install.sh"));
    command
        .arg("--profile")
        .arg(&draft.path)
        .arg("--replace-profile");
    if draft.action == 2 {
        command.arg("--no-start");
    }
    if !gui {
        command.arg("--no-gui");
    }
    if asio {
        command.arg("--with-asio");
    }
    section(out, "Environment")?;
    for key in ["PREFIX", "DESTDIR", "SIDEALSA_SOCKET", "ALSA_PLUGIN_DIR"] {
        row(out, key, format_args!("{:?}", std::env::var_os(key)))?;
    }
    writeln!(
        out,
        "{}",
        dim(
            "Unset/empty DESTDIR means LIVE system installation. A non-root staging destination leaves services unchanged. PREFIX overrides /usr/local binary/data paths."
        )
    )?;
    summary(out, &draft.path, &draft.text)?;
    section(out, "Review")?;
    row(
        out,
        "Command",
        format_args!("{command:?} (arguments, NOT shell)"),
    )?;
    row(
        out,
        "Select",
        format_args!(
            "/etc/sidealsa/active.toml -> /etc/sidealsa/profiles/{} (replace)",
            draft.path.file_name().unwrap_or_default().to_string_lossy()
        ),
    )?;
    row(
        out,
        "Options",
        format_args!("GUI: {gui}; ASIO: {asio}; PipeWire integration regenerated"),
    )?;
    row(
        out,
        "Service",
        format_args!(
            "{}",
            if staged {
                "NONE: staged files only, no system service changes"
            } else if draft.action == 2 {
                "ENABLE FUTURE BOOTS; no immediate start/restart, running daemon remains running"
            } else {
                "RESTART NOW, interrupt audio, ENABLE FUTURE BOOTS"
            }
        ),
    )?;
    confirm(input, out, "Run the installer now")?;
    if !command.status()?.success() {
        return Err("installer failed; saved profile remains available".into());
    }
    Ok(())
}

fn staged_destination(value: Option<&std::ffi::OsStr>) -> Result<bool> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    let path = Path::new(value);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| part == std::path::Component::ParentDir)
        || path.components().all(|part| {
            matches!(
                part,
                std::path::Component::RootDir | std::path::Component::CurDir
            )
        })
    {
        return Err("DESTDIR must be a non-root absolute staging path; unset it for an explicitly confirmed live install".into());
    }
    Ok(true)
}

fn run() -> Result<()> {
    let mut root = std::env::current_dir()?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "sidealsa-setup [--project-root PATH] [--list-devices]\nDefault: numbered interactive menus (TTY required). Supported USB devices use vendor profiles and default to install and restart; manual setup defaults to SAVE ONLY. Every confirmation defaults to no.\n--list-devices: control-only playback/capture and USB/profile listing, no writes or PCM opens.\n--help: offline, no enumeration. Run scripts/setup.sh as a normal user."
        );
        return Ok(());
    }
    let mut list = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--project-root" {
            root = PathBuf::from(args.next().ok_or("--project-root requires PATH")?);
        } else if arg == "--list-devices" {
            list = true;
        } else {
            return Err(format!("unknown argument: {arg:?}").into());
        }
    }
    if list {
        let entries = discover();
        let addr_width = entries
            .iter()
            .map(|e| format!("card{} DEV{}", e.card, e.device).len())
            .max()
            .unwrap_or(0);
        for direction in [Direction::Playback, Direction::Capture] {
            println!(
                "\n{}",
                bold(&format!("{direction:?} PCMs — control metadata only"))
            );
            for e in entries.iter().filter(|e| e.direction == direction) {
                let usb = e.usb.map_or_else(
                    || "unknown/non-USB".into(),
                    |(vid, pid)| format!("{vid:04x}:{pid:04x}"),
                );
                let profile = vendor_profile(e.usb).map_or_else(
                    || "no registry match".into(),
                    |(file, label)| format!("profiles/{file} ({label})"),
                );
                println!(
                    "  {:addr_width$}  {}  {:?}  USB {usb}  ·  {profile}",
                    format!("card{} DEV{}", e.card, e.device),
                    e.selector,
                    e.name,
                    addr_width = addr_width
                );
            }
        }
        println!(
            "\n{}",
            dim(
                "Channels, format, rate and periods NOT verified. Automatic profiles require same-card playback + capture DEV0. Missing devices may require manual selectors."
            )
        );
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "interactive setup requires a TTY; use --help or explicit --list-devices".into(),
        );
    }
    // No root re-exec: only the existing installer may request privileges later.
    if unsafe { libc::geteuid() } == 0 {
        return Err("run setup as a normal user, not root/sudo".into());
    }
    let root = root.canonicalize()?;
    if !root.join("scripts/install.sh").is_file() || !root.join("Cargo.toml").is_file() {
        return Err("installation menus need a SideALSA checkout; run scripts/setup.sh or pass --project-root PATH".into());
    }
    let mut existing = Vec::new();
    let mut directories = vec![
        root.join("profiles"),
        root.join(STAGING_DIR),
        PathBuf::from("/etc/sidealsa/profiles"),
    ];
    println!("{}", bold("SideALSA setup"));
    match selection::installed_selection() {
        Ok(Some(s)) => {
            println!("{}", bold("Installed state (read only)"));
            println!("  profile  {}", s.profile.display());
            println!("  socket   {}", s.socket.display());
        }
        Ok(None) => {
            println!("{}", bold("Installed state (read only)"));
            println!("  No active.toml selection. Legacy service (not evaluated):");
            if let Ok(text) = fs::read_to_string("/etc/systemd/system/sidealsad.service") {
                for line in text.lines().filter(|s| s.starts_with("ExecStart=")) {
                    println!("  {line:?}");
                }
            }
        }
        Err(e) => println!("Installed selection unavailable (left untouched): {e}"),
    }
    println!(
        "{}",
        dim(
            "Numbered menus: Enter = default · b = back · c/EOF = cancel. Nothing is saved or installed without an explicit y."
        )
    );
    for dir in directories.drain(..) {
        if let Ok(files) = fs::read_dir(dir) {
            for file in files.flatten() {
                let path = file.path();
                if file.file_type().is_ok_and(|kind| kind.is_file())
                    && path.extension().is_some_and(|s| s == "toml")
                    && let Ok(p) = Profile::from_path(&path)
                {
                    existing.push((path, p.device.name));
                }
            }
        }
    }
    existing.sort_by(|a, b| a.0.cmp(&b.0));
    let entries = discover();
    let mut input = io::stdin().lock();
    let mut out = io::stdout().lock();
    loop {
        match plan(&mut input, &mut out, &root, &entries, &existing) {
            Ok(draft) => {
                if draft.create {
                    if draft.replace {
                        save_replace(&root, &draft.path, &draft.text)?;
                    } else {
                        save_new(&draft.path, &draft.text)?;
                    }
                    existing.push((
                        draft.path.clone(),
                        Profile::from_toml(&draft.text)?.device.name,
                    ));
                }
                writeln!(out, "Profile retained at {}", draft.path.display())?;
                if draft.action != 1 {
                    match install(&mut input, &mut out, &root, &draft) {
                        Err(e)
                            if matches!(e.downcast_ref::<Navigation>(), Some(Navigation::Back)) =>
                        {
                            continue;
                        }
                        result => result?,
                    }
                }
                return Ok(());
            }
            Err(e) if matches!(e.downcast_ref::<Navigation>(), Some(Navigation::Back)) => continue,
            Err(e) => return Err(e),
        }
    }
}

fn main() {
    if let Err(e) = run() {
        if e.downcast_ref::<Navigation>().is_some() {
            println!("Cancelled; installer not invoked. Any already-saved draft remains.");
        } else {
            eprintln!("sidealsa-setup: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn usb_entries(card: i32, pid: u16) -> Vec<Entry> {
        [Direction::Playback, Direction::Capture]
            .into_iter()
            .map(|direction| Entry {
                card,
                device: 0,
                usb: Some((0x152a, pid)),
                direction,
                selector: format!("hw:CARD=OTG{card},DEV=0"),
                name: "USB Audio".into(),
            })
            .collect()
    }

    #[test]
    fn sysfs_canonical_ancestry_and_nearest_invalid_identity() {
        let root = std::env::temp_dir().join(format!("sidealsa-sysfs-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let usb = root.join("usb");
        let interface = usb.join("interface");
        fs::create_dir_all(interface.join("sound/card0")).unwrap();
        fs::write(usb.join("idVendor"), "152A\n").unwrap();
        fs::write(usb.join("idProduct"), "8755\n").unwrap();
        let link = root.join("device");
        std::os::unix::fs::symlink(interface.join("sound/card0"), &link).unwrap();
        assert_eq!(usb_identity(&link), Some((0x152a, 0x8755)));
        fs::write(interface.join("idVendor"), "152a").unwrap();
        for malformed in ["oops", "87555", "", "+755", "87 5"] {
            fs::write(interface.join("idProduct"), malformed).unwrap();
            assert_eq!(usb_identity(&link), None);
        }
        fs::remove_file(interface.join("idProduct")).unwrap();
        assert_eq!(usb_identity(&link), None);
        assert_eq!(usb_identity(&root), None);
        assert_eq!(usb_identity(&root.join("missing")), None);
        fs::write(interface.join("idProduct"), "8756").unwrap();
        assert_eq!(usb_identity(&link), Some((0x152a, 0x8756)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_and_same_card_dev_zero_duplex_required() {
        for usb in [
            None,
            Some((0x152a, 0x8752)),
            Some((0x152a, 0xffff)),
            Some((0xffff, 0x8755)),
        ] {
            assert!(vendor_profile(usb).is_none());
        }
        assert!(
            vendor_profile(Some((0x152a, 0x8756)))
                .unwrap()
                .1
                .contains("UNVERIFIED")
        );
        let mut entries = usb_entries(1, 0x8755);
        entries.extend(usb_entries(2, 0x8755));
        assert_eq!(
            supported(&entries)
                .iter()
                .map(|e| e.card)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        entries[1].device = 1;
        assert_eq!(supported(&entries).len(), 1);
        entries[3].card = 3;
        assert!(supported(&entries).is_empty());
        assert!(supported(&usb_entries(4, 0x8756)[1..]).is_empty());
    }

    #[test]
    fn vendor_binding_preserves_every_other_field() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for pid in [0x8755, 0x8756] {
            let (file, _) = vendor_profile(Some((0x152a, pid))).unwrap();
            let original = fs::read_to_string(root.join("profiles").join(file)).unwrap();
            let bound = bind_vendor(&original, "hw:CARD=OTG_2,DEV=0").unwrap();
            let parsed = Profile::from_toml(&bound).unwrap();
            assert_eq!(parsed.device.playback.device, "hw:CARD=OTG_2,DEV=0");
            assert_eq!(parsed.device.capture.device, parsed.device.playback.device);
            let original_doc: DocumentMut = original.parse().unwrap();
            let mut restored: DocumentMut = bound.parse().unwrap();
            for direction in ["playback", "capture"] {
                restored["device"][direction]["device"] =
                    original_doc["device"][direction]["device"].clone();
            }
            assert_eq!(restored.to_string(), original);
        }
    }

    #[test]
    fn supported_plan_is_automatic_but_requires_confirmation_and_card_choice() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut entries = usb_entries(1, 0x8755);
        let mut out = Vec::new();
        let draft = plan(&mut Cursor::new("\n\ny\n"), &mut out, &root, &entries, &[]).unwrap();
        assert_eq!(draft.action, 3);
        assert!(draft.create);
        assert!(draft.replace);
        assert_eq!(
            draft.path,
            root.join("profiles/local/topping-e1x2-card1-local.toml")
        );
        let output = String::from_utf8(out).unwrap();
        assert!(!output.contains("Confirm proposed"));
        assert!(!output.contains("New draft path"));
        assert!(!output.contains("Explicit hw:"));
        for input in [
            "",
            "c\n",
            "b\n",
            "\n",
            "\n\n",
            "\n\n\n",
            "\n\nc\n",
            "\n\nn\n",
            "\n\nwrong\n",
        ] {
            assert!(
                plan(
                    &mut Cursor::new(input),
                    &mut Vec::new(),
                    &root,
                    &entries,
                    &[]
                )
                .is_err()
            );
        }
        entries.extend(usb_entries(2, 0x8756));
        assert!(
            plan(
                &mut Cursor::new("\n"),
                &mut Vec::new(),
                &root,
                &entries,
                &[]
            )
            .is_err()
        );
        let draft = plan(
            &mut Cursor::new("2\n1\ny\n"),
            &mut Vec::new(),
            &root,
            &entries,
            &[],
        )
        .unwrap();
        assert_eq!(draft.action, 1);
        assert_eq!(
            Profile::from_toml(&draft.text)
                .unwrap()
                .device
                .playback
                .device,
            "hw:CARD=OTG2,DEV=0"
        );
    }

    #[test]
    fn staging_path_is_stable_per_card_and_replace_stays_inside_staging() {
        let root = std::env::temp_dir().join(format!("sidealsa-path-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("profiles")).unwrap();
        fs::create_dir(root.join("profiles/local")).unwrap();
        // Same card always maps to the same file: no numbered pile-up.
        let first = local_path(&root, "topping-e1x2.toml", 2);
        let second = local_path(&root, "topping-e1x2.toml", 2);
        assert_eq!(first, second);
        assert_eq!(
            first,
            root.join("profiles/local/topping-e1x2-card2-local.toml")
        );
        assert_ne!(first, local_path(&root, "topping-e1x2.toml", 3));
        // Replace overwrites our own staging file, including over symlinks.
        let text = generate("hw:A,0", "hw:B,0", 2, 2).unwrap();
        std::os::unix::fs::symlink("missing", &first).unwrap();
        save_replace(&root, &first, &text).unwrap();
        assert_eq!(fs::read_to_string(&first).unwrap(), text);
        // Replace refuses to touch anything outside profiles/local.
        let outside = root.join("profiles/topping-e1x2.toml");
        fs::write(&outside, &text).unwrap();
        assert!(save_replace(&root, &outside, &text).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), text);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn menu_marks_default_and_requires_explicit_choice() {
        let mut out = Vec::new();
        let choice = menu(
            &mut Cursor::new("2\n"),
            &mut out,
            "Title",
            &["alpha".into(), "beta".into()],
            1,
        )
        .unwrap();
        assert_eq!(choice, 2);
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("← default"));
        // No default: empty input re-prompts instead of accepting.
        let mut out = Vec::new();
        let choice = menu(
            &mut Cursor::new("\n2\n"),
            &mut out,
            "Title",
            &["alpha".into(), "beta".into()],
            0,
        )
        .unwrap();
        assert_eq!(choice, 2);
        assert!(String::from_utf8(out).unwrap().contains("no default"));
    }

    #[test]
    fn formatted_summary_lists_aligned_key_rows_and_ports() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let text = fs::read_to_string(root.join("profiles/topping-e1x2.toml")).unwrap();
        let mut out = Vec::new();
        summary(&mut out, Path::new("profiles/topping-e1x2.toml"), &text).unwrap();
        let output = String::from_utf8(out).unwrap();
        for key in [
            "Profile summary",
            "Playback",
            "Capture",
            "48000 Hz",
            "line1",
        ] {
            assert!(output.contains(key), "missing {key:?}:\n{output}");
        }
    }

    #[test]
    fn destination_summary_never_calls_root_staging() {
        use std::ffi::OsStr;
        assert!(!staged_destination(None).unwrap());
        assert!(!staged_destination(Some(OsStr::new(""))).unwrap());
        assert!(staged_destination(Some(OsStr::new("/tmp/package"))).unwrap());
        for invalid in ["/", "//", "///", "/./", "/tmp/..", "relative"] {
            assert!(staged_destination(Some(OsStr::new(invalid))).is_err());
        }
    }

    #[test]
    fn fake_entries_generate_distinct_unlinked_fullchannel_profile() {
        let entries = [
            Entry {
                card: 0,
                device: 0,
                usb: None,
                direction: Direction::Playback,
                selector: "hw:CARD=Analog,DEV=0".into(),
                name: "Analog".into(),
            },
            Entry {
                card: 1,
                device: 2,
                usb: None,
                direction: Direction::Capture,
                selector: "hw:CARD=Mic,DEV=2".into(),
                name: "Mic".into(),
            },
        ];
        let mut out = Vec::new();
        let playback = selector(
            &mut Cursor::new("2\n"),
            &mut out,
            &entries,
            Direction::Playback,
        )
        .unwrap();
        let capture = selector(
            &mut Cursor::new("2\n"),
            &mut out,
            &entries,
            Direction::Capture,
        )
        .unwrap();
        let p = Profile::from_toml(&generate(&playback, &capture, 4, 2).unwrap()).unwrap();
        assert_ne!(p.device.playback.device, p.device.capture.device);
        assert!(!p.device.effective_duplex_link());
        assert_eq!(p.device.pro_latency_periods, 1);
        assert_eq!(p.ports.playback[0].id, "output");
        assert_eq!(p.ports.playback[0].channels, vec![0, 1, 2, 3]);
        assert_eq!(p.ports.capture[0].id, "input");
        assert_eq!(p.device.realtime_priority, 50);
        assert_eq!(p.device.pro_realtime_priority, Some(48));
        assert_eq!(p.device.rate, 48000);
        assert_eq!(p.device.period_size, 64);
        assert_eq!(p.device.effective_hardware_period_size(), 64);
        assert_eq!(p.device.buffer_size, 256);
        assert_eq!(p.device.effective_shared_buffer_size(), 512);
        assert!(p.device.startup_loopback.is_none());
    }

    #[test]
    fn unsafe_strings_are_quoted_not_toml_injected() {
        let s = "hw:CARD=a\";$(touch nope)\\,DEV=0";
        let text = generate(s, "hw:Other,1", 2, 2).unwrap();
        assert_eq!(Profile::from_toml(&text).unwrap().device.playback.device, s);
        assert!(generate("hw:a,0", "hw:b,0", 0, 2).is_err());
    }

    #[test]
    fn cancel_eof_and_back_produce_no_commit_plan() {
        for text in [
            "",
            "c\n",
            "b\n",
            "1\n",
            "1\n1\nhw:a,0\n1\nhw:b,0\n2\n2\n/tmp/sidealsa-never-save-draft.toml\n1\nc\n",
        ] {
            let result = plan(
                &mut Cursor::new(text),
                &mut Vec::new(),
                Path::new("/nonexistent"),
                &[],
                &[],
            );
            assert!(
                result
                    .err()
                    .is_some_and(|e| e.downcast_ref::<Navigation>().is_some())
            );
        }
    }

    #[test]
    fn save_existing_refused() {
        let path =
            std::env::temp_dir().join(format!("sidealsa-setup-test-{}.toml", std::process::id()));
        let text = generate("hw:A,0", "hw:B,0", 2, 2).unwrap();
        save_new(&path, &text).unwrap();
        assert!(save_new(&path, TEMPLATE).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("sidealsa-nonexistent-target", &path).unwrap();
        assert!(save_new(&path, &text).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn existing_profile_requires_explicit_selection_and_is_not_rewritten() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = root.join("profiles/generic-onboard-analog.toml");
        let existing = vec![(path.clone(), Profile::from_path(&path).unwrap().device.name)];
        let mut out = Vec::new();
        assert!(plan(&mut Cursor::new("2\n\n"), &mut out, &root, &[], &existing).is_err());
        let draft = plan(
            &mut Cursor::new("2\n1\n\ny\n"),
            &mut out,
            &root,
            &[],
            &existing,
        )
        .unwrap();
        assert!(!draft.create);
        assert_eq!(draft.action, 1);
        assert_eq!(draft.path, path);
        assert_eq!(draft.text, TEMPLATE);
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains(&existing[0].1));
        assert!(output.contains(&path.to_string_lossy().to_string()));
    }

    #[test]
    fn default_action_is_save_only_and_all_input_prefixes_cancel() {
        let answers = [
            "1",
            "1",
            "hw:A,0",
            "1",
            "hw:B,0",
            "",
            "",
            "/nonexistent/sidealsa-draft.toml",
            "",
            "y",
        ];
        for end in 0..answers.len() {
            let input = format!("{}\n", answers[..end].join("\n"));
            assert!(
                plan(
                    &mut Cursor::new(input),
                    &mut Vec::new(),
                    Path::new("/nonexistent"),
                    &[],
                    &[]
                )
                .is_err()
            );
        }
        let draft = plan(
            &mut Cursor::new(answers.join("\n") + "\n"),
            &mut Vec::new(),
            Path::new("/nonexistent"),
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(draft.action, 1);
        assert!(draft.create);
        assert_eq!(
            Profile::from_toml(&draft.text)
                .unwrap()
                .device
                .capture
                .channels,
            2
        );
    }

    #[test]
    fn installer_cancel_or_eof_never_executes() {
        let draft = Draft {
            path: PathBuf::from("/nonexistent/draft.toml"),
            text: generate("hw:Playback,0", "hw:Capture,0", 2, 2).unwrap(),
            create: false,
            action: 2,
            replace: false,
        };
        for input in ["", "c\n", "1\n", "1\n1\n", "1\n1\nwrong\n", "1\n2\nc\n"] {
            let error = install(
                &mut Cursor::new(input),
                &mut Vec::new(),
                Path::new("/nonexistent"),
                &draft,
            )
            .unwrap_err();
            assert!(error.downcast_ref::<Navigation>().is_some());
        }
    }

    #[test]
    fn placeholders_are_rejected_before_installation_confirmation() {
        let draft = Draft {
            path: "/nonexistent/draft.toml".into(),
            text: TEMPLATE.into(),
            create: false,
            action: 2,
            replace: false,
        };
        let error = install(
            &mut Cursor::new("1\n1\ny\n"),
            &mut Vec::new(),
            Path::new("/nonexistent"),
            &draft,
        )
        .unwrap_err();
        assert!(error.to_string().contains("placeholders"));
    }
}
