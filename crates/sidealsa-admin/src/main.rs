use std::{env, path::PathBuf, process::ExitCode, time::Duration};

use sidealsa_admin::{
    APPLY_LOCK_PATH, AdminError, ApplyLock, ApplyOutcome, DEFAULT_SOCKET_PATH, LEGACY_PROFILE_PATH,
    SystemdRuntime, apply_transaction, parse_timing_assignments, read_snapshot, render_snapshot,
    validate_managed_profile_path,
};
use sidealsa_config::selection::{InstalledSelection, installed_selection};

const CLIENT_REFRESH_REQUIRED_ERROR_EXIT_CODE: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Show,
    Apply,
}

struct Args {
    command: Command,
    profile: PathBuf,
    socket: PathBuf,
    expected_revision: Option<String>,
    assignments: Vec<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let exit_code = error_exit_code(&error);
            eprintln!("{error}");
            ExitCode::from(exit_code)
        }
    }
}

fn error_exit_code(error: &AdminError) -> u8 {
    match error {
        AdminError::Runtime(_)
        | AdminError::RolledBack { .. }
        | AdminError::RollbackFailed { .. } => CLIENT_REFRESH_REQUIRED_ERROR_EXIT_CODE,
        _ => 1,
    }
}

fn run() -> Result<(), AdminError> {
    let args = parse_args()?;
    validate_managed_profile_path(&args.profile)?;
    match args.command {
        Command::Show => {
            if !args.assignments.is_empty() || args.expected_revision.is_some() {
                return Err(AdminError::InvalidArgument(
                    "show does not accept timing assignments or --expected-revision".into(),
                ));
            }
            print!("{}", render_snapshot(&args.profile, &args.socket)?);
        }
        Command::Apply => {
            if unsafe { libc::geteuid() } != 0 {
                return Err(AdminError::InvalidArgument(
                    "apply must run as root through pkexec".into(),
                ));
            }
            let expected_revision = args.expected_revision.ok_or_else(|| {
                AdminError::InvalidArgument("apply requires --expected-revision".into())
            })?;
            if args.assignments.is_empty() {
                return Err(AdminError::InvalidArgument(
                    "apply requires at least one timing key=value".into(),
                ));
            }
            let _lock = ApplyLock::acquire(APPLY_LOCK_PATH)?;
            let snapshot = read_snapshot(&args.profile)?;
            let timing = parse_timing_assignments(snapshot.timing, &args.assignments)?;
            let mut runtime = SystemdRuntime::new(&args.socket, Duration::from_secs(12));
            match apply_transaction(&args.profile, &expected_revision, &timing, &mut runtime)? {
                ApplyOutcome::Unchanged => println!("configuration unchanged; daemon restarted"),
                ApplyOutcome::Applied => println!("configuration applied"),
            }
        }
    }
    Ok(())
}

fn parse_args() -> Result<Args, AdminError> {
    parse_args_from(
        env::args().skip(1),
        || Ok(installed_selection()?),
        || {
            // Only pre-selection installations may use this concrete legacy profile.
            match std::fs::symlink_metadata(LEGACY_PROFILE_PATH) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error.into()),
                Ok(_) => {
                    validate_managed_profile_path(std::path::Path::new(LEGACY_PROFILE_PATH))?;
                    Ok(true)
                }
            }
        },
    )
}

fn parse_args_from(
    mut arguments: impl Iterator<Item = String>,
    selection: impl FnOnce() -> Result<Option<InstalledSelection>, AdminError>,
    legacy_exists: impl FnOnce() -> Result<bool, AdminError>,
) -> Result<Args, AdminError> {
    let command = match arguments.next().as_deref() {
        Some("show") => Command::Show,
        Some("apply") => Command::Apply,
        Some("--help" | "-h") => {
            print_help();
            std::process::exit(0);
        }
        Some(value) => {
            return Err(AdminError::InvalidArgument(format!(
                "unknown command '{value}'"
            )));
        }
        None => {
            return Err(AdminError::InvalidArgument(
                "expected show or apply command".into(),
            ));
        }
    };

    let mut profile = None;
    let mut socket = None;
    let mut expected_revision = None;
    let mut assignments = Vec::new();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--profile" => {
                profile = Some(PathBuf::from(next_value(&mut arguments, "--profile")?));
            }
            "--socket" => {
                socket = Some(PathBuf::from(next_value(&mut arguments, "--socket")?));
            }
            "--expected-revision" => {
                expected_revision = Some(next_value(&mut arguments, "--expected-revision")?);
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            value if value.contains('=') => assignments.push(value.to_string()),
            value => {
                return Err(AdminError::InvalidArgument(format!(
                    "unknown argument '{value}'"
                )));
            }
        }
    }
    if (profile.is_none() || socket.is_none())
        && let Some(selected) = selection()?
    {
        profile.get_or_insert(selected.profile);
        socket.get_or_insert(selected.socket);
    }
    let profile = match profile {
        Some(profile) => profile,
        None if legacy_exists()? => PathBuf::from(LEGACY_PROFILE_PATH),
        None => {
            return Err(AdminError::InvalidArgument(
                "no installed selection or legacy profile; use --profile PATH".into(),
            ));
        }
    };
    let socket = socket.unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH));
    if !socket.is_absolute() {
        return Err(AdminError::InvalidArgument(
            "socket path must be absolute".into(),
        ));
    }
    Ok(Args {
        command,
        profile,
        socket,
        expected_revision,
        assignments,
    })
}

fn next_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, AdminError> {
    arguments
        .next()
        .ok_or_else(|| AdminError::InvalidArgument(format!("{option} requires a value")))
}

fn print_help() {
    println!(
        "Defaults: trusted /etc/sidealsa/active.toml; legacy profile only if installed; otherwise use --profile PATH."
    );
    println!("sidealsa-admin show [--profile PATH] [--socket PATH]");
    println!(
        "sidealsa-admin apply --expected-revision HASH [--profile PATH] [--socket PATH] key=value ..."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_precedence_and_absence() {
        for (flags, selected, legacy, expected) in [
            (
                vec!["show"],
                true,
                false,
                Some(("/selected.toml", "/selected.sock")),
            ),
            (
                vec!["show", "--profile", "/explicit.toml"],
                true,
                false,
                Some(("/explicit.toml", "/selected.sock")),
            ),
            (
                vec!["show", "--socket", "/explicit.sock"],
                true,
                false,
                Some(("/selected.toml", "/explicit.sock")),
            ),
            (
                vec!["show", "--profile", "/explicit.toml"],
                false,
                false,
                Some(("/explicit.toml", DEFAULT_SOCKET_PATH)),
            ),
            (
                vec!["show"],
                false,
                true,
                Some((LEGACY_PROFILE_PATH, DEFAULT_SOCKET_PATH)),
            ),
            (vec!["show"], false, false, None),
        ] {
            let result = parse_args_from(
                flags.into_iter().map(String::from),
                || {
                    Ok(selected.then(|| InstalledSelection {
                        profile: "/selected.toml".into(),
                        socket: "/selected.sock".into(),
                    }))
                },
                || Ok(legacy),
            );
            if let Some((profile, socket)) = expected {
                let args = result.unwrap();
                assert_eq!(args.profile, PathBuf::from(profile));
                assert_eq!(args.socket, PathBuf::from(socket));
            } else {
                assert!(result.err().unwrap().to_string().contains("use --profile"));
            }
        }
        parse_args_from(
            [
                "show",
                "--profile",
                "/explicit.toml",
                "--socket",
                "/explicit.sock",
            ]
            .into_iter()
            .map(String::from),
            || panic!("selection must not be read"),
            || panic!("legacy must not be checked"),
        )
        .unwrap();
        let error = parse_args_from(
            ["show", "--profile", "/explicit.toml"]
                .into_iter()
                .map(String::from),
            || Err(AdminError::InvalidArgument("untrusted selection".into())),
            || panic!("must not fall back"),
        );
        assert!(
            error
                .err()
                .unwrap()
                .to_string()
                .contains("untrusted selection")
        );
    }

    #[test]
    fn client_refresh_errors_have_distinct_exit_code() {
        assert_eq!(error_exit_code(&AdminError::Runtime("failed".into())), 2);
        assert_eq!(
            error_exit_code(&AdminError::RolledBack {
                cause: "failed".into()
            }),
            2
        );
        assert_eq!(
            error_exit_code(&AdminError::RollbackFailed {
                cause: "failed".into(),
                rollback: "failed".into(),
            }),
            2
        );
    }

    #[test]
    fn pre_restart_errors_keep_generic_exit_code() {
        assert_eq!(error_exit_code(&AdminError::RevisionConflict), 1);
        assert_eq!(
            error_exit_code(&AdminError::InvalidArgument("bad value".into())),
            1
        );
    }
}
