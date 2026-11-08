pub mod config;
pub mod error;
pub mod git;
pub mod log;

use std::{
    fs::{self},
    path::{Path, PathBuf},
};

use crate::{config::RepositoriesConfigurationFile, error::ZlorbError, log::Logger};

pub fn get_home_dir() -> PathBuf {
    match std::env::home_dir() {
        Some(x) => x,
        None => {
            Logger::error("Failed to get the home directory".into());
            panic!("Program exited due to previous error");
        }
    }
}

pub fn explicit_config_dir_from(
    zlorb_config_dir: Option<&str>,
    xdg_config_home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(dir) = zlorb_config_dir {
        let trimmed = dir.trim();
        if !trimmed.is_empty() && std::path::Path::new(trimmed).is_absolute() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Some(dir) = xdg_config_home {
        let trimmed = dir.trim();
        if !trimmed.is_empty() && std::path::Path::new(trimmed).is_absolute() {
            return Some(PathBuf::from(trimmed).join("zlorb"));
        }
    }
    None
}

pub fn resolve_config_dir_from(
    zlorb_config_dir: Option<&str>,
    xdg_config_home: Option<&str>,
    home_dir: &std::path::Path,
) -> PathBuf {
    match explicit_config_dir_from(zlorb_config_dir, xdg_config_home) {
        Some(dir) => dir,
        None => home_dir.join(".config").join("zlorb"),
    }
}

pub fn get_zlorb_config_dir() -> PathBuf {
    let zlorb_config_dir = std::env::var("ZLORB_CONFIG_DIR").ok();
    let xdg_config_home = std::env::var("XDG_CONFIG_HOME").ok();
    if let Some(dir) = explicit_config_dir_from(
        zlorb_config_dir.as_deref(),
        xdg_config_home.as_deref(),
    ) {
        return dir;
    }
    resolve_config_dir_from(None, None, &get_home_dir())
}

pub fn effective_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    line.split_whitespace().nth(2)?.parse().ok()
}

pub fn is_root() -> bool {
    match effective_uid() {
        Some(uid) => uid == 0,
        None => std::env::var("UID").map(|u| u == "0").unwrap_or(false),
    }
}

/// gets the toml file for zlorb repo configurations
/// File: ~/.config/zlorb/repositories.toml
pub fn get_zlorb_repo_config_file() -> PathBuf {
    get_zlorb_config_dir().join("repositories.toml")
}

pub fn create_file_with_content(path: PathBuf, content: &String) -> Result<String, ZlorbError> {
    std::fs::write(path, content).map_err(ZlorbError::Io)?;
    Ok(content.clone())
}

pub fn read_file_from_filesystem(path: PathBuf) -> Option<String> {
    let f = std::fs::read_to_string(path);
    if f.is_err() {
        return None;
    }
    Some(f.unwrap())
}

pub fn check_file_exist(path: PathBuf) -> bool {
    std::fs::exists(path).unwrap_or_default()
}

pub fn load_config_from_path(
    config_file_path: &Path,
) -> Result<RepositoriesConfigurationFile, ZlorbError> {
    let file_contents = match fs::read_to_string(config_file_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RepositoriesConfigurationFile {
                repositories: vec![],
            });
        }
        Err(e) => return Err(ZlorbError::Io(e)),
    };
    if file_contents.trim().is_empty() {
        return Ok(RepositoriesConfigurationFile {
            repositories: vec![],
        });
    }
    toml::from_str::<RepositoriesConfigurationFile>(&file_contents).map_err(|e| {
        ZlorbError::ConfigParseError(format!("{}: {}", config_file_path.display(), e))
    })
}

pub fn load_config_from_toml() -> Result<RepositoriesConfigurationFile, ZlorbError> {
    load_config_from_path(&get_zlorb_repo_config_file())
}

pub fn save_config_to_path(
    config_file_path: &Path,
    config: &RepositoriesConfigurationFile,
) -> Result<(), ZlorbError> {
    let out_toml = toml::to_string(config).map_err(ZlorbError::TomlSerializationError)?;
    if let Some(parent) = config_file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file_name = config_file_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repositories.toml".to_string());
    let tmp_path = config_file_path.with_file_name(format!(
        "{}.tmp-{}",
        file_name,
        std::process::id()
    ));
    let existing_permissions = fs::metadata(config_file_path)
        .map(|metadata| metadata.permissions())
        .ok();
    let write_result = (|| -> Result<(), ZlorbError> {
        let mut tmp_file = fs::File::create(&tmp_path).map_err(ZlorbError::Io)?;
        std::io::Write::write_all(&mut tmp_file, out_toml.as_bytes())
            .map_err(ZlorbError::Io)?;
        tmp_file.sync_all().map_err(ZlorbError::Io)?;
        drop(tmp_file);
        if let Some(permissions) = existing_permissions {
            fs::set_permissions(&tmp_path, permissions).map_err(ZlorbError::Io)?;
        }
        fs::rename(&tmp_path, config_file_path).map_err(ZlorbError::Io)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_result
}

pub fn save_config_to_toml(
    config: &RepositoriesConfigurationFile,
) -> Result<(), ZlorbError> {
    save_config_to_path(&get_zlorb_repo_config_file(), config)
}

pub struct ConfigFileLock {
    #[cfg(unix)]
    file: fs::File,
}

pub fn config_lock_path(config_file_path: &Path) -> PathBuf {
    let file_name = config_file_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repositories.toml".to_string());
    config_file_path.with_file_name(format!("{}.lock", file_name))
}

pub fn lock_config_file(config_file_path: &Path) -> Result<ConfigFileLock, ZlorbError> {
    if let Some(parent) = config_file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let lock_path = config_lock_path(config_file_path);
        let file = match fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                fs::File::open(&lock_path).map_err(ZlorbError::Io)?
            }
            Err(e) => return Err(ZlorbError::Io(e)),
        };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(ZlorbError::Io(std::io::Error::last_os_error()));
        }
        Ok(ConfigFileLock { file })
    }
    #[cfg(not(unix))]
    {
        Ok(ConfigFileLock {})
    }
}

pub fn lock_repo_config() -> Result<ConfigFileLock, ZlorbError> {
    lock_config_file(&get_zlorb_repo_config_file())
}

#[cfg(unix)]
impl Drop for ConfigFileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn explicit_config_dir_wins_over_xdg_and_home() {
        let dir = resolve_config_dir_from(
            Some("/srv/zlorb"),
            Some("/home/u/.config"),
            Path::new("/home/u"),
        );
        assert_eq!(dir, PathBuf::from("/srv/zlorb"));
    }

    #[test]
    fn xdg_config_home_is_used_when_no_explicit_dir() {
        let dir = resolve_config_dir_from(None, Some("/xdg"), Path::new("/home/u"));
        assert_eq!(dir, PathBuf::from("/xdg/zlorb"));
    }

    #[test]
    fn home_config_is_the_fallback() {
        let dir = resolve_config_dir_from(None, None, Path::new("/home/u"));
        assert_eq!(dir, PathBuf::from("/home/u/.config/zlorb"));
    }

    #[test]
    fn relative_explicit_dir_is_ignored() {
        let dir = resolve_config_dir_from(
            Some("relative/zlorb"),
            Some("/xdg"),
            Path::new("/home/u"),
        );
        assert_eq!(dir, PathBuf::from("/xdg/zlorb"));
    }

    #[test]
    fn relative_xdg_is_ignored() {
        let dir = resolve_config_dir_from(None, Some("relative"), Path::new("/home/u"));
        assert_eq!(dir, PathBuf::from("/home/u/.config/zlorb"));
    }

    #[test]
    fn blank_explicit_dir_falls_through_to_xdg() {
        let dir = resolve_config_dir_from(Some("  "), Some("/xdg"), Path::new("/home/u"));
        assert_eq!(dir, PathBuf::from("/xdg/zlorb"));
    }

    fn temp_config_path(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "zlorb-lib-config-test-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("repositories.toml")
    }

    fn sample_config() -> RepositoriesConfigurationFile {
        RepositoriesConfigurationFile {
            repositories: vec![crate::config::RepositoryConfiguration {
                name: "demo".to_string(),
                path: "/tmp/demo".to_string(),
                remote: "origin".to_string(),
                branch: "master".to_string(),
                build_strategy: None,
                build_command: String::new(),
                restart_command: None,
                out_dir: None,
            }],
        }
    }

    #[test]
    fn missing_config_file_loads_as_empty() {
        let path = temp_config_path("missing");
        let loaded = load_config_from_path(&path).unwrap();
        assert!(loaded.repositories.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn empty_and_comment_only_config_load_as_empty() {
        let path = temp_config_path("empty");
        fs::write(&path, "").unwrap();
        assert!(load_config_from_path(&path).unwrap().repositories.is_empty());
        fs::write(&path, "# no repos yet\n").unwrap();
        assert!(load_config_from_path(&path).unwrap().repositories.is_empty());
    }

    #[test]
    fn malformed_config_is_an_error_naming_the_file_and_leaves_it_unchanged() {
        let path = temp_config_path("malformed");
        let bad = "this is not = valid = toml [[\n";
        fs::write(&path, bad).unwrap();
        let err = load_config_from_path(&path).unwrap_err();
        let rendered = format!("{:?}", err);
        assert!(
            rendered.contains("repositories.toml"),
            "error should name the file: {}",
            rendered
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), bad);
    }

    #[test]
    fn config_with_a_wrong_table_name_is_an_error_not_an_empty_config() {
        let path = temp_config_path("wrong-key");
        let contents = "[[repositories]]\nname = \"demo\"\npath = \"/tmp/demo\"\nremote = \"origin\"\nbranch = \"master\"\n";
        fs::write(&path, contents).unwrap();
        assert!(load_config_from_path(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }

    #[test]
    fn config_entry_with_an_unknown_field_is_an_error_not_silently_dropped() {
        let path = temp_config_path("unknown-field");
        let contents = "[[repository]]\nname = \"demo\"\npath = \"/tmp/demo\"\nremote = \"origin\"\nbranch = \"master\"\ncustom_thing = \"keepme\"\n";
        fs::write(&path, contents).unwrap();
        assert!(load_config_from_path(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_the_existing_files_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_config_path("permissions");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        save_config_to_path(&path, &sample_config()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn save_then_load_roundtrips_and_fully_replaces_previous_contents() {
        let path = temp_config_path("roundtrip");
        save_config_to_path(&path, &sample_config()).unwrap();
        let loaded = load_config_from_path(&path).unwrap();
        assert_eq!(loaded.repositories.len(), 1);
        assert_eq!(loaded.repositories[0].name, "demo");
        let empty = RepositoriesConfigurationFile {
            repositories: vec![],
        };
        save_config_to_path(&path, &empty).unwrap();
        let loaded = load_config_from_path(&path).unwrap();
        assert!(loaded.repositories.is_empty());
        assert!(
            !fs::read_to_string(&path).unwrap().contains("demo"),
            "stale contents survived a full replace"
        );
    }

    #[test]
    fn config_lock_path_is_a_sidecar_next_to_the_config_file() {
        let path = Path::new("/tmp/example/repositories.toml");
        assert_eq!(
            config_lock_path(path),
            PathBuf::from("/tmp/example/repositories.toml.lock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_lock_is_exclusive_until_the_guard_is_dropped() {
        use std::sync::mpsc;
        let path = temp_config_path("lock-exclusive");
        let first = lock_config_file(&path).unwrap();
        assert!(config_lock_path(&path).exists());

        let (acquired_tx, acquired_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let contender_path = path.clone();
        let contender = std::thread::spawn(move || {
            let _second = lock_config_file(&contender_path).unwrap();
            acquired_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });

        assert!(
            acquired_rx
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "second lock was granted while the first guard was still held"
        );
        drop(first);
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("second lock was not granted after the first guard dropped");
        release_tx.send(()).unwrap();
        contender.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn config_lock_survives_the_config_file_being_replaced_by_a_save() {
        use std::sync::mpsc;
        let path = temp_config_path("lock-survives-save");
        let first = lock_config_file(&path).unwrap();
        save_config_to_path(&path, &sample_config()).unwrap();

        let (acquired_tx, acquired_rx) = mpsc::channel::<()>();
        let contender_path = path.clone();
        let contender = std::thread::spawn(move || {
            let _second = lock_config_file(&contender_path).unwrap();
            acquired_tx.send(()).unwrap();
        });
        assert!(
            acquired_rx
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "saving the config (rename) released the lock"
        );
        drop(first);
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("second lock was not granted after the first guard dropped");
        contender.join().unwrap();
    }
}
