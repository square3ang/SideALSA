use sidealsa_config::{
    Profile,
    integration::{compatible_topology, render_alsa, render_pipewire},
    selection::InstalledSelection,
};
use std::{collections::HashMap, env, fs, path::PathBuf};

fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        println!(
            "sidealsa-config-gen --profile FILE --socket PATH --output-dir DIR [--alsa-only] [--check-compatible-profile OLD] [--installed-profile PATH]"
        );
        println!("sidealsa-config-gen --selected-profile FILE | --selected-socket FILE");
        println!("Offline generation only; no daemon connection or audio device access.");
        return Ok(());
    }
    if args.len() == 2 && matches!(args[0].as_str(), "--selected-profile" | "--selected-socket") {
        let selected = InstalledSelection::from_path(&args[1])?;
        println!(
            "{}",
            if args[0] == "--selected-profile" {
                selected.profile
            } else {
                selected.socket
            }
            .display()
        );
        return Ok(());
    }
    let mut values = HashMap::new();
    let mut alsa_only = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--alsa-only" && !alsa_only {
            alsa_only = true;
            continue;
        }
        if !matches!(
            arg.as_str(),
            "--profile"
                | "--socket"
                | "--output-dir"
                | "--check-compatible-profile"
                | "--installed-profile"
        ) {
            return Err(format!("unknown or duplicate argument: {arg}").into());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {arg}"))?;
        if values.insert(arg.clone(), value).is_some() {
            return Err(format!("duplicate argument: {arg}").into());
        }
    }
    let required = |key: &str| {
        values
            .get(key)
            .ok_or_else(|| format!("required argument: {key}"))
    };
    let profile = Profile::from_path(required("--profile")?)?;
    let socket = required("--socket")?;
    let output = PathBuf::from(required("--output-dir")?);
    let alsa = render_alsa(&profile, socket)?;
    let pipewire = if alsa_only {
        None
    } else {
        Some(render_pipewire(&profile)?)
    };
    if let Some(old) = values.get("--check-compatible-profile")
        && !compatible_topology(&Profile::from_path(old)?, &profile)
    {
        return Err("incompatible profile topology: port IDs, directions, channel counts or positions differ".into());
    }
    let selection = values
        .get("--installed-profile")
        .map(|path| {
            InstalledSelection {
                profile: PathBuf::from(path),
                socket: PathBuf::from(socket),
            }
            .to_toml()
        })
        .transpose()?;
    // All input validation and rendering precedes any output creation.
    fs::create_dir_all(&output)?;
    fs::write(output.join("asound.sidealsa.conf"), alsa)?;
    if let Some(pipewire) = pipewire {
        fs::write(output.join("pipewire.conf"), pipewire)?;
    }
    if let Some(selection) = selection {
        fs::write(output.join("active.toml"), selection)?;
    }
    Ok(())
}

fn main() {
    if let Err(error) = run(env::args().skip(1).collect()) {
        eprintln!("sidealsa-config-gen: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_generation_validates_before_writing() {
        let dir = env::temp_dir().join(format!("sidealsa-gen-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let profile = dir.join("profile.toml");
        let out = dir.join("out");
        let args = || {
            vec![
                "--profile".into(),
                profile.display().to_string(),
                "--socket".into(),
                "/tmp/test.sock".into(),
                "--output-dir".into(),
                out.display().to_string(),
            ]
        };
        fs::write(&profile, "invalid = true").unwrap();
        assert!(run(args()).is_err());
        assert!(!out.exists());
        fs::write(
            &profile,
            include_str!("../../../../profiles/topping-e1x2.toml"),
        )
        .unwrap();
        let mut invalid = args();
        invalid.extend(["--installed-profile".into(), "relative.toml".into()]);
        assert!(run(invalid).is_err());
        assert!(!out.exists());
        let old = dir.join("old.toml");
        fs::write(
            &old,
            include_str!("../../../../profiles/topping-e1x2.toml")
                .replace("id = \"line1\"", "id = \"other\""),
        )
        .unwrap();
        let mut incompatible = args();
        incompatible.extend([
            "--check-compatible-profile".into(),
            old.display().to_string(),
        ]);
        assert!(
            run(incompatible)
                .unwrap_err()
                .to_string()
                .contains("incompatible profile topology")
        );
        assert!(!out.exists());
        let mut valid = args();
        valid.extend([
            "--alsa-only".into(),
            "--installed-profile".into(),
            "/etc/sidealsa/profiles/generic.toml".into(),
        ]);
        run(valid).unwrap();
        assert!(out.join("asound.sidealsa.conf").is_file());
        assert!(!out.join("pipewire.conf").exists());
        let selection = InstalledSelection::from_path(out.join("active.toml")).unwrap();
        assert_eq!(
            selection.profile,
            PathBuf::from("/etc/sidealsa/profiles/generic.toml")
        );
        for query in ["--selected-profile", "--selected-socket"] {
            run(vec![
                query.into(),
                out.join("active.toml").display().to_string(),
            ])
            .unwrap();
        }
        run(args()).unwrap();
        assert!(out.join("pipewire.conf").is_file());
        fs::remove_dir_all(dir).unwrap();
    }
}
