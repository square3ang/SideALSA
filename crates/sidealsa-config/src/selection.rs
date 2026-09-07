//! Installed selection parsing and trusted runtime loading.
use crate::ProfileError;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const INSTALLED_SELECTION_PATH: &str = "/etc/sidealsa/active.toml";
pub const LEGACY_PROFILE_PATH: &str = "/etc/sidealsa/profiles/topping-e1x2.toml";
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/sidealsad.sock";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstalledSelection {
    pub profile: PathBuf,
    pub socket: PathBuf,
}

pub(crate) fn validate_path(path: &Path) -> Result<(), ProfileError> {
    if !path.is_absolute()
        || path
            .to_str()
            .is_none_or(|s| s.chars().any(char::is_control))
    {
        return Err(ProfileError::Invalid(
            "selection/socket paths must be absolute UTF-8 paths without control characters".into(),
        ));
    }
    Ok(())
}

impl InstalledSelection {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ProfileError> {
        Self::from_toml(&fs::read_to_string(path)?)
    }

    pub fn from_toml(text: &str) -> Result<Self, ProfileError> {
        let selection: Self = toml::from_str(text)?;
        selection.validate()?;
        Ok(selection)
    }

    fn validate(&self) -> Result<(), ProfileError> {
        validate_path(&self.profile)?;
        validate_path(&self.socket)
    }

    pub fn to_toml(&self) -> Result<String, ProfileError> {
        self.validate()?;
        toml::to_string(self).map_err(|e| ProfileError::Invalid(e.to_string()))
    }
}

/// Load a trusted selection and validate its managed profile target.
/// The target is not opened or pinned; see [`validate_installed_profile`] for
/// the remaining race between metadata validation and a consumer's later open.
pub fn installed_selection() -> Result<Option<InstalledSelection>, ProfileError> {
    for directory in [Path::new("/"), Path::new("/etc")] {
        validate_directory(directory)?;
    }
    trusted_selection(
        Path::new(INSTALLED_SELECTION_PATH),
        Path::new("/etc/sidealsa/profiles"),
    )
}

/// Validate a profile for runtime use, including an explicit legacy fallback.
///
/// These metadata checks do not pin the inode: a later profile open can race
/// replacement by a privileged writer. Consumers needing an atomic snapshot
/// must validate and read through the same securely opened file descriptor.
pub fn validate_installed_profile(path: &Path) -> Result<(), ProfileError> {
    for directory in [Path::new("/"), Path::new("/etc")] {
        validate_directory(directory)?;
    }
    validate_profile_target(path, Path::new("/etc/sidealsa/profiles"))
}

fn validate_directory(path: &Path) -> Result<(), ProfileError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(ProfileError::Invalid(format!(
            "installed profile directory '{}' must be a root-owned directory, not a symlink or group/world writable",
            path.display()
        )));
    }
    Ok(())
}

fn validate_profile_target(path: &Path, profiles_dir: &Path) -> Result<(), ProfileError> {
    validate_path(path)?;
    let filename = path.file_name().and_then(|name| name.to_str());
    // Compare raw spelling: Path component comparisons normalize '.' and '//'.
    if filename.is_none_or(|name| {
        name.strip_suffix(".toml").is_none_or(str::is_empty)
            || path.as_os_str() != profiles_dir.join(name).as_os_str()
    }) {
        return Err(ProfileError::Invalid(format!(
            "installed profile must be a direct child of '{}' with a nonempty stem and .toml extension, without traversal",
            profiles_dir.display()
        )));
    }
    validate_directory(profiles_dir.parent().ok_or_else(|| {
        ProfileError::Invalid("installed profiles directory has no parent".into())
    })?)?;
    validate_directory(profiles_dir)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(ProfileError::Invalid(
            "installed profile must be a root-owned regular file, not a symlink or group/world writable".into(),
        ));
    }
    Ok(())
}

fn trusted_selection(
    path: &Path,
    profiles_dir: &Path,
) -> Result<Option<InstalledSelection>, ProfileError> {
    // O_NOFOLLOW rejects even dangling symlinks; O_NONBLOCK avoids hanging on FIFOs.
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    validate_directory(path.parent().ok_or_else(|| {
        ProfileError::Invalid("installed selection has no parent directory".into())
    })?)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(ProfileError::Invalid(
            "installed selection must be a root-owned regular file, not group/world writable"
                .into(),
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let selection = InstalledSelection::from_toml(&text)?;
    validate_profile_target(&selection.profile, profiles_dir)?;
    Ok(Some(selection))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn selection_roundtrip_and_validation() {
        let selection = InstalledSelection {
            profile: "/etc/a\"b\\c.toml".into(),
            socket: "/tmp/socket with spaces".into(),
        };
        assert_eq!(
            InstalledSelection::from_toml(&selection.to_toml().unwrap()).unwrap(),
            selection
        );
        for bad in ["relative", "/tmp/\n", "/tmp/\0"] {
            let mut invalid = selection.clone();
            invalid.profile = bad.into();
            assert!(invalid.to_toml().is_err());
            invalid.profile = selection.profile.clone();
            invalid.socket = bad.into();
            assert!(invalid.to_toml().is_err());
        }
        for text in [
            "profile = '/etc/profile'",
            "profile = 'relative'\nsocket = '/tmp/s'",
            "profile = '/etc/p'\nsocket = '/tmp/s'\nunknown = 1",
        ] {
            assert!(InstalledSelection::from_toml(text).is_err());
        }
    }

    #[test]
    fn trusted_metadata_and_untrusted_staging() {
        let dir = std::env::temp_dir().join(format!("sidealsa-selection-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let profiles = dir.join("profiles");
        fs::create_dir(&profiles).unwrap();
        fs::set_permissions(&profiles, fs::Permissions::from_mode(0o755)).unwrap();
        let target = profiles.join("generic.toml");
        fs::write(&target, "").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let path = dir.join("active.toml");
        assert!(trusted_selection(&path, &profiles).unwrap().is_none());
        fs::write(
            &path,
            InstalledSelection {
                profile: target.clone(),
                socket: "/tmp/socket".into(),
            }
            .to_toml()
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        InstalledSelection::from_path(&path).unwrap();
        assert!(trusted_selection(&path, &profiles).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            trusted_selection(&path, &profiles).is_ok(),
            fs::metadata(&path).unwrap().uid() == 0
        );
        let link = dir.join("link");
        symlink(&path, &link).unwrap();
        assert!(trusted_selection(&link, &profiles).is_err());
        fs::remove_file(&path).unwrap();
        assert!(trusted_selection(&link, &profiles).is_err());
        assert!(trusted_selection(&dir, &profiles).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn runtime_target_restrictions() {
        let dir =
            std::env::temp_dir().join(format!("sidealsa-profile-trust-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let profiles = dir.join("profiles");
        fs::create_dir(&profiles).unwrap();
        fs::set_permissions(&profiles, fs::Permissions::from_mode(0o755)).unwrap();
        let target = profiles.join("generic.toml");
        fs::write(&target, "").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let root_owned = fs::metadata(&target).unwrap().uid() == 0;
        assert_eq!(
            validate_profile_target(&target, &profiles).is_ok(),
            root_owned
        );
        for invalid in [
            dir.join("outside.toml"),
            profiles.join("../outside.toml"),
            profiles.join("./generic.toml"),
            profiles.join("sub/../generic.toml"),
            profiles.join(".toml"),
            profiles.join("generic.TOML"),
            profiles.join("generic"),
            profiles.join("generic.toml/"),
        ] {
            assert!(
                validate_profile_target(&invalid, &profiles).is_err(),
                "{}",
                invalid.display()
            );
        }
        let link = profiles.join("link.toml");
        symlink(&target, &link).unwrap();
        assert!(validate_profile_target(&link, &profiles).is_err());
        let directory_target = profiles.join("directory.toml");
        fs::create_dir(&directory_target).unwrap();
        assert!(validate_profile_target(&directory_target, &profiles).is_err());
        for mode in [0o664, 0o646] {
            fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
            assert!(validate_profile_target(&target, &profiles).is_err());
        }
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        for directory in [&profiles, &dir] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o775)).unwrap();
            assert!(validate_profile_target(&target, &profiles).is_err());
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let real_profiles = dir.join("real-profiles");
        fs::rename(&profiles, &real_profiles).unwrap();
        symlink(&real_profiles, &profiles).unwrap();
        assert!(validate_profile_target(&target, &profiles).is_err());
        fs::remove_file(&profiles).unwrap();
        fs::rename(&real_profiles, &profiles).unwrap();
        let parent_link = dir.with_extension("link");
        symlink(&dir, &parent_link).unwrap();
        assert!(
            validate_profile_target(
                &parent_link.join("profiles/generic.toml"),
                &parent_link.join("profiles")
            )
            .is_err()
        );
        fs::remove_file(&parent_link).unwrap();

        // A trusted selection cannot redirect runtime loading outside the tree.
        let selection_path = dir.join("active.toml");
        let selection = InstalledSelection {
            profile: dir.join("outside.toml"),
            socket: "/tmp/socket".into(),
        };
        fs::write(&selection_path, selection.to_toml().unwrap()).unwrap();
        fs::set_permissions(&selection_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            InstalledSelection::from_path(&selection_path).unwrap(),
            selection
        );
        assert!(trusted_selection(&selection_path, &profiles).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
