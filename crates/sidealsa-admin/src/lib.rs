use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::MetadataExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sidealsa_client::SideAlsaClient;
use sidealsa_config::{Profile, ProfileDocument, ProfileError, TimingSettings};
use sidealsa_protocol::DeviceInfo;
use thiserror::Error;

pub const DEFAULT_PROFILE_PATH: &str = "/etc/sidealsa/profiles/topping-e1x2.toml";
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/sidealsad.sock";
pub const PROFILE_ROOT: &str = "/etc/sidealsa/profiles";
pub const APPLY_LOCK_PATH: &str = "/run/sidealsa-admin.lock";
const SERVICE_NAME: &str = "sidealsad.service";

#[derive(Debug, Error)]
pub enum AdminError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("profile changed since it was loaded; reload the GUI and try again")]
    RevisionConflict,
    #[error("could not restart sidealsad with the saved profile: {0}")]
    Runtime(String),
    #[error("could not apply profile: {cause}; original profile restored")]
    RolledBack { cause: String },
    #[error("could not apply profile: {cause}; rollback failed: {rollback}")]
    RollbackFailed { cause: String, rollback: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileSnapshot {
    pub timing: TimingSettings,
    pub revision: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Unchanged,
    Applied,
}

pub trait RuntimeControl {
    fn restart(&mut self) -> Result<(), String>;
    fn wait_until_ready(&mut self, profile: &Profile) -> Result<(), String>;
}

pub struct SystemdRuntime {
    socket: PathBuf,
    previous_pid: Option<u32>,
    timeout: Duration,
}

pub struct ApplyLock {
    file: File,
}

#[derive(Debug)]
struct AtomicReplaceError {
    source: io::Error,
    committed: bool,
}

impl SystemdRuntime {
    pub fn new(socket: impl Into<PathBuf>, timeout: Duration) -> Self {
        let socket = socket.into();
        let previous_pid = service_main_pid().ok().flatten();
        Self {
            socket,
            previous_pid,
            timeout,
        }
    }
}

impl RuntimeControl for SystemdRuntime {
    fn restart(&mut self) -> Result<(), String> {
        let output = Command::new("/usr/bin/systemctl")
            .args(["restart", SERVICE_NAME])
            .output()
            .map_err(|error| format!("could not execute systemctl: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if message.is_empty() {
            format!("systemctl exited with {}", output.status)
        } else {
            message
        })
    }

    fn wait_until_ready(&mut self, profile: &Profile) -> Result<(), String> {
        let deadline = Instant::now() + self.timeout;
        let mut last_error = "daemon socket is unavailable".to_string();
        while Instant::now() < deadline {
            match SideAlsaClient::connect_with_timeout(&self.socket, Duration::from_millis(250)) {
                Ok(mut client) => {
                    let pid = match trusted_daemon_pid(&client) {
                        Ok(pid) => pid,
                        Err(error) => {
                            last_error = error;
                            thread::sleep(Duration::from_millis(50));
                            continue;
                        }
                    };
                    match client.get_info() {
                        Ok(_) if Some(pid) == self.previous_pid => {
                            last_error = "daemon did not restart".into();
                        }
                        Ok(info) if info_matches_profile(&info, profile) => {
                            self.previous_pid = Some(pid);
                            return Ok(());
                        }
                        Ok(_) => last_error = "daemon loaded different timing values".into(),
                        Err(error) => last_error = error.to_string(),
                    }
                }
                Err(error) => last_error = error.to_string(),
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(format!("timed out waiting for sidealsad: {last_error}"))
    }
}

impl ApplyLock {
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, AdminError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self { file })
    }
}

impl Drop for ApplyLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn validate_managed_profile_path(path: &Path) -> Result<(), AdminError> {
    if !path.is_absolute() || path.parent() != Some(Path::new(PROFILE_ROOT)) {
        return Err(AdminError::InvalidArgument(format!(
            "profile must be a direct child of {PROFILE_ROOT}"
        )));
    }
    if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
        return Err(AdminError::InvalidArgument(
            "profile must have a .toml extension".into(),
        ));
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(AdminError::InvalidArgument(
            "profile must be a regular file, not a symlink".into(),
        ));
    }
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(AdminError::InvalidArgument(
            "profile must be root-owned and not group/world-writable".into(),
        ));
    }
    let parent_metadata = fs::metadata(PROFILE_ROOT)?;
    if parent_metadata.uid() != 0 || parent_metadata.mode() & 0o022 != 0 {
        return Err(AdminError::InvalidArgument(
            "profile directory must be root-owned and not group/world-writable".into(),
        ));
    }
    Ok(())
}

pub fn read_snapshot(path: impl AsRef<Path>) -> Result<ProfileSnapshot, AdminError> {
    let text = fs::read_to_string(path)?;
    let document = ProfileDocument::from_toml(&text)?;
    Ok(ProfileSnapshot {
        timing: document.timing(),
        revision: profile_revision(&text),
    })
}

pub fn render_snapshot(
    path: impl AsRef<Path>,
    socket: impl AsRef<Path>,
) -> Result<String, AdminError> {
    let path = path.as_ref();
    let text = fs::read_to_string(path)?;
    let document = ProfileDocument::from_toml(&text)?;
    let mut output = String::new();
    push_setting(&mut output, "revision", &profile_revision(&text));
    push_timing(&mut output, &document.timing());

    match SideAlsaClient::connect_with_timeout(socket, Duration::from_millis(250)) {
        Ok(mut client) => match (trusted_daemon_pid(&client), client.get_info()) {
            (Ok(pid), Ok(info)) => {
                push_setting(&mut output, "daemon_status", "active");
                push_setting(&mut output, "daemon_pid", &pid.to_string());
                push_setting(&mut output, "daemon_rate", &info.rate.to_string());
                push_setting(
                    &mut output,
                    "daemon_period_size",
                    &info.period_size.to_string(),
                );
                push_setting(
                    &mut output,
                    "daemon_hardware_period_size",
                    &info.hardware_period_size.to_string(),
                );
                push_setting(
                    &mut output,
                    "daemon_buffer_size",
                    &info.buffer_size.to_string(),
                );
                push_setting(
                    &mut output,
                    "daemon_profile_matches",
                    if info_matches_profile(&info, document.profile()) {
                        "true"
                    } else {
                        "false"
                    },
                );
            }
            (_, Err(error)) => {
                push_setting(&mut output, "daemon_status", "error");
                push_setting(
                    &mut output,
                    "daemon_error",
                    &single_line(&error.to_string()),
                );
            }
            (Err(error), _) => {
                push_setting(&mut output, "daemon_status", "error");
                push_setting(
                    &mut output,
                    "daemon_error",
                    &single_line(&error.to_string()),
                );
            }
        },
        Err(error) => {
            push_setting(&mut output, "daemon_status", "unavailable");
            push_setting(
                &mut output,
                "daemon_error",
                &single_line(&error.to_string()),
            );
        }
    }
    Ok(output)
}

pub fn parse_timing_assignments(
    mut timing: TimingSettings,
    assignments: &[String],
) -> Result<TimingSettings, AdminError> {
    let mut seen = HashSet::new();
    for assignment in assignments {
        let (key, value) = assignment.split_once('=').ok_or_else(|| {
            AdminError::InvalidArgument(format!("expected key=value, got '{assignment}'"))
        })?;
        if !seen.insert(key) {
            return Err(AdminError::InvalidArgument(format!(
                "setting '{key}' was provided more than once"
            )));
        }
        match key {
            "rate" => timing.rate = parse_u32(key, value)?,
            "period_size" => timing.period_size = parse_u32(key, value)?,
            "hardware_period_size" => timing.hardware_period_size = parse_optional_u32(key, value)?,
            "buffer_size" => timing.buffer_size = parse_u32(key, value)?,
            "shared_buffer_size" => timing.shared_buffer_size = parse_optional_u32(key, value)?,
            "playback_queue_periods" => {
                timing.playback_queue_periods = parse_optional_u32(key, value)?
            }
            "playback_timer_scheduling" => {
                timing.playback_timer_scheduling = parse_bool(key, value)?
            }
            "duplex_link" => timing.duplex_link = parse_optional_bool(key, value)?,
            "linked_playback_guard_frames" => {
                timing.linked_playback_guard_frames = parse_optional_u32(key, value)?
            }
            "linked_phase_max_attempts" => {
                timing.linked_phase_max_attempts = parse_u32(key, value)?
            }
            "pro_latency_periods" => timing.pro_latency_periods = parse_u32(key, value)?,
            "pro_handoff_us" => timing.pro_handoff_us = parse_u32(key, value)?,
            "pro_realtime_priority" => {
                timing.pro_realtime_priority = parse_optional_u32(key, value)?
            }
            "shared_latency_periods" => timing.shared_latency_periods = parse_u32(key, value)?,
            "shared_playback_repeat_on_underrun" => {
                timing.shared_playback_repeat_on_underrun = parse_bool(key, value)?
            }
            "realtime" => timing.realtime = parse_bool(key, value)?,
            "realtime_priority" => timing.realtime_priority = parse_u32(key, value)?,
            _ => {
                return Err(AdminError::InvalidArgument(format!(
                    "unknown timing setting '{key}'"
                )));
            }
        }
    }
    Ok(timing)
}

pub fn apply_transaction(
    path: impl AsRef<Path>,
    expected_revision: &str,
    timing: &TimingSettings,
    runtime: &mut impl RuntimeControl,
) -> Result<ApplyOutcome, AdminError> {
    let path = path.as_ref();
    let original_text = fs::read_to_string(path)?;
    if profile_revision(&original_text) != expected_revision {
        return Err(AdminError::RevisionConflict);
    }
    let original = ProfileDocument::from_toml(&original_text)?;
    if &original.timing() == timing {
        runtime
            .restart()
            .and_then(|()| runtime.wait_until_ready(original.profile()))
            .map_err(AdminError::Runtime)?;
        return Ok(ApplyOutcome::Unchanged);
    }

    let mut candidate = original.clone();
    candidate.apply_timing(timing)?;
    if let Err(error) = atomic_replace(path, candidate.to_toml().as_bytes()) {
        if !error.committed {
            return Err(AdminError::Io(error.source));
        }
        return Err(rollback_after_failure(
            path,
            &original_text,
            original.profile(),
            runtime,
            format!(
                "profile was replaced, but its directory could not be synchronized: {}",
                error.source
            ),
        ));
    }

    let apply_result = runtime
        .restart()
        .and_then(|()| runtime.wait_until_ready(candidate.profile()));
    if let Err(cause) = apply_result {
        return Err(rollback_after_failure(
            path,
            &original_text,
            original.profile(),
            runtime,
            cause,
        ));
    }
    Ok(ApplyOutcome::Applied)
}

pub fn profile_revision(text: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn atomic_replace(path: &Path, contents: &[u8]) -> Result<(), AtomicReplaceError> {
    let parent = path.parent().ok_or_else(|| {
        AtomicReplaceError::before(io::Error::new(
            io::ErrorKind::InvalidInput,
            "profile has no parent",
        ))
    })?;
    let metadata = fs::metadata(path).map_err(AtomicReplaceError::before)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("profile.toml");
    let temporary = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));

    let mut committed = false;
    let write_result = (|| -> Result<(), io::Error> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(metadata.mode() & 0o7777)
            .open(&temporary)?;
        if unsafe { libc::fchown(file.as_raw_fd(), metadata.uid(), metadata.gid()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        committed = true;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result.map_err(|source| AtomicReplaceError { source, committed })
}

fn info_matches_profile(info: &DeviceInfo, profile: &Profile) -> bool {
    let device = &profile.device;
    info.profile_fingerprint == profile.fingerprint()
        && info.rate == device.rate
        && info.period_size == device.period_size
        && info.hardware_period_size == device.effective_hardware_period_size()
        && info.buffer_size == device.buffer_size
        && info.shared_buffer_size == device.effective_shared_buffer_size()
        && info.pro_latency_periods == device.pro_latency_periods
        && info.pro_output_latency_frames == device.effective_pro_output_latency_frames()
        && info.pro_realtime_priority == device.effective_pro_realtime_priority()
        && info.shared_latency_periods == device.shared_latency_periods
}

fn rollback_after_failure(
    path: &Path,
    original_text: &str,
    original_profile: &Profile,
    runtime: &mut impl RuntimeControl,
    cause: String,
) -> AdminError {
    match restore_original(path, original_text, original_profile, runtime) {
        Ok(()) => AdminError::RolledBack { cause },
        Err(rollback) => AdminError::RollbackFailed { cause, rollback },
    }
}

fn restore_original(
    path: &Path,
    original_text: &str,
    original_profile: &Profile,
    runtime: &mut impl RuntimeControl,
) -> Result<(), String> {
    let durability_error = match atomic_replace(path, original_text.as_bytes()) {
        Ok(()) => None,
        Err(error) if error.committed => Some(format!(
            "original profile was replaced, but its directory could not be synchronized: {}",
            error.source
        )),
        Err(error) => {
            return Err(format!(
                "could not restore original profile: {}",
                error.source
            ));
        }
    };
    runtime.restart()?;
    runtime.wait_until_ready(original_profile)?;
    match durability_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn service_main_pid() -> Result<Option<u32>, String> {
    let output = Command::new("/usr/bin/systemctl")
        .args(["show", "--property=MainPID", "--value", SERVICE_NAME])
        .output()
        .map_err(|error| format!("could not query systemd MainPID: {error}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if message.is_empty() {
            format!("systemctl show exited with {}", output.status)
        } else {
            message
        });
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let pid = value
        .parse::<u32>()
        .map_err(|_| format!("systemd returned invalid MainPID '{value}'"))?;
    Ok((pid != 0).then_some(pid))
}

fn trusted_daemon_pid(client: &SideAlsaClient) -> Result<u32, String> {
    let credentials = client
        .peer_credentials()
        .map_err(|error| format!("could not read daemon credentials: {error}"))?;
    if credentials.uid != 0 {
        return Err(format!(
            "daemon socket peer UID is {}, expected root",
            credentials.uid
        ));
    }
    let service_pid =
        service_main_pid()?.ok_or_else(|| format!("{SERVICE_NAME} is not running"))?;
    if credentials.pid != service_pid {
        return Err(format!(
            "daemon socket peer PID {} does not match systemd MainPID {service_pid}",
            credentials.pid
        ));
    }
    Ok(credentials.pid)
}

impl AtomicReplaceError {
    fn before(source: io::Error) -> Self {
        Self {
            source,
            committed: false,
        }
    }
}

fn push_timing(output: &mut String, timing: &TimingSettings) {
    push_setting(output, "rate", &timing.rate.to_string());
    push_setting(output, "period_size", &timing.period_size.to_string());
    push_setting(
        output,
        "hardware_period_size",
        &optional_u32(timing.hardware_period_size),
    );
    push_setting(output, "buffer_size", &timing.buffer_size.to_string());
    push_setting(
        output,
        "shared_buffer_size",
        &optional_u32(timing.shared_buffer_size),
    );
    push_setting(
        output,
        "playback_queue_periods",
        &optional_u32(timing.playback_queue_periods),
    );
    push_setting(
        output,
        "playback_timer_scheduling",
        bool_text(timing.playback_timer_scheduling),
    );
    push_setting(output, "duplex_link", optional_bool(timing.duplex_link));
    push_setting(
        output,
        "linked_playback_guard_frames",
        &optional_u32(timing.linked_playback_guard_frames),
    );
    push_setting(
        output,
        "linked_phase_max_attempts",
        &timing.linked_phase_max_attempts.to_string(),
    );
    push_setting(
        output,
        "pro_latency_periods",
        &timing.pro_latency_periods.to_string(),
    );
    push_setting(output, "pro_handoff_us", &timing.pro_handoff_us.to_string());
    push_setting(
        output,
        "pro_realtime_priority",
        &optional_u32(timing.pro_realtime_priority),
    );
    push_setting(
        output,
        "shared_latency_periods",
        &timing.shared_latency_periods.to_string(),
    );
    push_setting(
        output,
        "shared_playback_repeat_on_underrun",
        bool_text(timing.shared_playback_repeat_on_underrun),
    );
    push_setting(output, "realtime", bool_text(timing.realtime));
    push_setting(
        output,
        "realtime_priority",
        &timing.realtime_priority.to_string(),
    );
}

fn push_setting(output: &mut String, key: &str, setting: &str) {
    output.push_str(key);
    output.push('=');
    output.push_str(setting);
    output.push('\n');
}

fn optional_u32(setting: Option<u32>) -> String {
    setting.map_or_else(|| "auto".into(), |value| value.to_string())
}

fn optional_bool(setting: Option<bool>) -> &'static str {
    setting.map_or("auto", bool_text)
}

fn bool_text(setting: bool) -> &'static str {
    if setting { "true" } else { "false" }
}

fn parse_u32(key: &str, setting: &str) -> Result<u32, AdminError> {
    setting.parse().map_err(|_| {
        AdminError::InvalidArgument(format!("'{setting}' is not a valid value for {key}"))
    })
}

fn parse_optional_u32(key: &str, setting: &str) -> Result<Option<u32>, AdminError> {
    if setting == "auto" {
        Ok(None)
    } else {
        parse_u32(key, setting).map(Some)
    }
}

fn parse_bool(key: &str, setting: &str) -> Result<bool, AdminError> {
    match setting {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(AdminError::InvalidArgument(format!(
            "'{setting}' is not true or false for {key}"
        ))),
    }
}

fn parse_optional_bool(key: &str, setting: &str) -> Result<Option<bool>, AdminError> {
    if setting == "auto" {
        Ok(None)
    } else {
        parse_bool(key, setting).map(Some)
    }
}

fn single_line(message: &str) -> String {
    message.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const PROFILE: &str = include_str!("../../../profiles/topping-e1x2.toml");
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct FakeRuntime {
        restart_count: usize,
        wait_count: usize,
        fail_first_wait: bool,
        fail_second_wait: bool,
    }

    impl RuntimeControl for FakeRuntime {
        fn restart(&mut self) -> Result<(), String> {
            self.restart_count += 1;
            Ok(())
        }

        fn wait_until_ready(&mut self, _profile: &Profile) -> Result<(), String> {
            self.wait_count += 1;
            if (self.fail_first_wait && self.wait_count == 1)
                || (self.fail_second_wait && self.wait_count == 2)
            {
                Err("candidate hardware open failed".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn parses_every_timing_assignment() {
        let document = ProfileDocument::from_toml(PROFILE).expect("profile should parse");
        let assignments = vec![
            "rate=44100".into(),
            "period_size=128".into(),
            "hardware_period_size=64".into(),
            "buffer_size=384".into(),
            "shared_buffer_size=1024".into(),
            "playback_queue_periods=2".into(),
            "playback_timer_scheduling=false".into(),
            "duplex_link=auto".into(),
            "linked_playback_guard_frames=auto".into(),
            "linked_phase_max_attempts=0".into(),
            "pro_latency_periods=2".into(),
            "pro_handoff_us=500".into(),
            "pro_realtime_priority=40".into(),
            "shared_latency_periods=4".into(),
            "shared_playback_repeat_on_underrun=false".into(),
            "realtime=false".into(),
            "realtime_priority=60".into(),
        ];

        let timing = parse_timing_assignments(document.timing(), &assignments)
            .expect("assignments should parse");

        assert_eq!(timing.rate, 44_100);
        assert_eq!(timing.period_size, 128);
        assert_eq!(timing.hardware_period_size, Some(64));
        assert_eq!(timing.duplex_link, None);
        assert_eq!(timing.linked_playback_guard_frames, None);
        assert!(!timing.playback_timer_scheduling);
        assert!(!timing.shared_playback_repeat_on_underrun);
        assert!(!timing.realtime);
    }

    #[test]
    fn rejects_unknown_or_duplicate_assignments() {
        let document = ProfileDocument::from_toml(PROFILE).expect("profile should parse");
        let error = parse_timing_assignments(document.timing(), &["latency=4".into()])
            .expect_err("unknown setting should fail");
        assert!(error.to_string().contains("unknown timing setting"));

        let error = parse_timing_assignments(
            document.timing(),
            &["rate=48000".into(), "rate=44100".into()],
        )
        .expect_err("duplicate setting should fail");
        assert!(error.to_string().contains("more than once"));
    }

    #[test]
    fn transaction_applies_valid_profile() {
        let directory = test_directory();
        let profile_path = directory.join("profile.toml");
        fs::write(&profile_path, PROFILE).expect("profile should write");
        let snapshot = read_snapshot(&profile_path).expect("snapshot should load");
        let mut timing = snapshot.timing;
        timing.rate = 44_100;
        let mut runtime = FakeRuntime {
            restart_count: 0,
            wait_count: 0,
            fail_first_wait: false,
            fail_second_wait: false,
        };

        let outcome = apply_transaction(&profile_path, &snapshot.revision, &timing, &mut runtime)
            .expect("profile should apply");

        assert_eq!(outcome, ApplyOutcome::Applied);
        assert_eq!(runtime.restart_count, 1);
        assert_eq!(runtime.wait_count, 1);
        assert_eq!(
            ProfileDocument::from_path(&profile_path)
                .expect("updated profile should parse")
                .timing()
                .rate,
            44_100
        );
        fs::remove_dir_all(directory).expect("test directory should remove");
    }

    #[test]
    fn transaction_restores_original_profile_when_daemon_rejects_candidate() {
        let directory = test_directory();
        let profile_path = directory.join("profile.toml");
        fs::write(&profile_path, PROFILE).expect("profile should write");
        let snapshot = read_snapshot(&profile_path).expect("snapshot should load");
        let mut timing = snapshot.timing;
        timing.rate = 44_100;
        let mut runtime = FakeRuntime {
            restart_count: 0,
            wait_count: 0,
            fail_first_wait: true,
            fail_second_wait: false,
        };

        let error = apply_transaction(&profile_path, &snapshot.revision, &timing, &mut runtime)
            .expect_err("failed hardware apply should roll back");

        assert!(error.to_string().contains("original profile restored"));
        assert_eq!(runtime.restart_count, 2);
        assert_eq!(runtime.wait_count, 2);
        assert_eq!(
            fs::read_to_string(&profile_path).expect("profile should read"),
            PROFILE
        );
        fs::remove_dir_all(directory).expect("test directory should remove");
    }

    #[test]
    fn transaction_rejects_stale_revision_before_writing() {
        let directory = test_directory();
        let profile_path = directory.join("profile.toml");
        fs::write(&profile_path, PROFILE).expect("profile should write");
        let timing = ProfileDocument::from_toml(PROFILE)
            .expect("profile should parse")
            .timing();
        let mut runtime = FakeRuntime {
            restart_count: 0,
            wait_count: 0,
            fail_first_wait: false,
            fail_second_wait: false,
        };

        let error = apply_transaction(&profile_path, "stale", &timing, &mut runtime)
            .expect_err("stale revision should fail");

        assert!(matches!(error, AdminError::RevisionConflict));
        assert_eq!(runtime.restart_count, 0);
        fs::remove_dir_all(directory).expect("test directory should remove");
    }

    #[test]
    fn unchanged_timing_restarts_and_verifies_saved_profile() {
        let directory = test_directory();
        let profile_path = directory.join("profile.toml");
        fs::write(&profile_path, PROFILE).expect("profile should write");
        let snapshot = read_snapshot(&profile_path).expect("snapshot should load");
        let mut runtime = FakeRuntime {
            restart_count: 0,
            wait_count: 0,
            fail_first_wait: false,
            fail_second_wait: false,
        };

        let outcome = apply_transaction(
            &profile_path,
            &snapshot.revision,
            &snapshot.timing,
            &mut runtime,
        )
        .expect("saved profile should restart");

        assert_eq!(outcome, ApplyOutcome::Unchanged);
        assert_eq!(runtime.restart_count, 1);
        assert_eq!(runtime.wait_count, 1);
        fs::remove_dir_all(directory).expect("test directory should remove");
    }

    #[test]
    fn transaction_reports_failed_rollback_verification_truthfully() {
        let directory = test_directory();
        let profile_path = directory.join("profile.toml");
        fs::write(&profile_path, PROFILE).expect("profile should write");
        let snapshot = read_snapshot(&profile_path).expect("snapshot should load");
        let mut timing = snapshot.timing;
        timing.rate = 44_100;
        let mut runtime = FakeRuntime {
            restart_count: 0,
            wait_count: 0,
            fail_first_wait: true,
            fail_second_wait: true,
        };

        let error = apply_transaction(&profile_path, &snapshot.revision, &timing, &mut runtime)
            .expect_err("failed rollback verification should be reported");

        assert!(matches!(error, AdminError::RollbackFailed { .. }));
        assert!(error.to_string().contains("rollback failed"));
        assert_eq!(
            fs::read_to_string(&profile_path).expect("profile should read"),
            PROFILE
        );
        fs::remove_dir_all(directory).expect("test directory should remove");
    }

    fn test_directory() -> PathBuf {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sidealsa-admin-test-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("test directory should create");
        path
    }
}
