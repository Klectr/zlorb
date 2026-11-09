use std::{env, path::Path, process};
use zlorb_lib::{
    config::{RepositoriesConfigurationFile, RepositoryConfiguration},
    error::ZlorbError,
    git::detect_repo,
    is_root,
    load_config_from_toml,
    lock_repo_config,
    log::Logger,
    save_config_to_toml,
};

pub(crate) fn watch() -> Result<(), ZlorbError> {
    let is_root = is_root();

    let mut cmd = process::Command::new("journalctl");
    cmd.arg("-f");
    if is_root {
        cmd.arg("-u").arg("zlorb");
    } else {
        cmd.arg("--user").arg("-u").arg("zlorb");
    }

    watch_with(cmd.status().map_err(ZlorbError::Io))
}

fn watch_with(outcome: Result<process::ExitStatus, ZlorbError>) -> Result<(), ZlorbError> {
    let status = outcome?;
    if status.success() {
        println!("End");
        Ok(())
    } else {
        Err(ZlorbError::Other(format!(
            "journalctl exited with {}; the log stream did not end cleanly",
            status
        )))
    }
}

pub(crate) fn remove(repo_name: String) -> Result<(), ZlorbError> {
    let _config_lock = lock_repo_config()?;
    let mut config = load_config_from_toml()?;
    if !config
        .repositories
        .iter()
        .any(|repo| repo.name == repo_name)
    {
        Logger::info(format!(
            "repo '{}' is not tracked; nothing to remove",
            repo_name
        ));
        return Ok(());
    }

    let filtered_repos = config
        .repositories
        .into_iter()
        .filter(|repo| repo.name != repo_name)
        .collect();
    config.repositories = filtered_repos;
    save_config_to_toml(&config)?;
    Ok(())
}

pub(crate) fn add() -> Result<(), ZlorbError> {
    let cwd = env::current_dir()?;
    let detected = match detect_repo(&cwd) {
        Ok(detected) => detected,
        Err(detect_error) => {
            let Some(root) = git2::Repository::discover(&cwd)
                .ok()
                .and_then(|repo| repo.workdir().map(|dir| dir.to_path_buf()))
            else {
                return Err(detect_error);
            };
            let Some(root_str) = root.to_str() else {
                return Err(detect_error);
            };
            let _config_lock = lock_repo_config()?;
            let config = load_config_from_toml()?;
            if let Some(tracked_name) = tracked_name_for_path(root_str, &config) {
                Logger::info(format!(
                    "{} is already in the configuration as '{}'; nothing to add",
                    root_str, tracked_name
                ));
                return Ok(());
            }
            return Err(detect_error);
        }
    };
    let path_str = detected
        .root
        .to_str()
        .ok_or_else(|| {
            ZlorbError::Other(format!(
                "{} is not valid UTF-8; zlorb stores repository paths as text, so it cannot watch a repository whose path is not valid UTF-8 — rename or move the repository and add again",
                detected.root.display()
            ))
        })?
        .to_string();

    let repo_name = detected
        .root
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            ZlorbError::Other(format!(
                "could not read a repository name from {}",
                path_str
            ))
        })?;

    let _config_lock = lock_repo_config()?;
    let mut config = load_config_from_toml()?;

    let refreshed_remote_branch = if detected.remote_branch != detected.branch {
        Some(detected.remote_branch.clone())
    } else {
        None
    };
    if let Some(tracked) = config
        .repositories
        .iter_mut()
        .find(|item| same_path(&item.path, &path_str))
    {
        if tracked.branch == detected.branch
            && tracked.remote == detected.remote
            && tracked.resolved_remote_branch() == detected.remote_branch
        {
            Logger::info(format!(
                "{} is already in the configuration as '{}'; nothing to add",
                path_str, tracked.name
            ));
            return Ok(());
        }
        let tracked_name = tracked.name.clone();
        let mut changes = Vec::new();
        if tracked.branch != detected.branch {
            changes.push(format!("branch to '{}'", detected.branch));
        }
        if tracked.remote != detected.remote {
            changes.push(format!("remote to '{}'", detected.remote));
        }
        if tracked.resolved_remote_branch() != detected.remote_branch {
            changes.push(format!("remote branch to '{}'", detected.remote_branch));
        }
        tracked.branch = detected.branch.clone();
        tracked.remote = detected.remote.clone();
        tracked.remote_branch = refreshed_remote_branch;
        save_config_to_toml(&config)?;
        Logger::info(format!(
            "{} is already in the configuration as '{}'; updated its tracked {}",
            path_str,
            tracked_name,
            changes.join(", ")
        ));
        return Ok(());
    }

    let name_collision = config
        .repositories
        .iter()
        .find(|item| item.name == repo_name)
        .map(|item| (item.name.clone(), item.path.clone()));
    let repo_name = unique_repo_name(&repo_name, &detected.root, &config);
    if let Some((colliding_name, colliding_path)) = name_collision {
        if repo_name != colliding_name {
            Logger::info(format!(
                "a different repository named '{}' is already tracked at {}; adding this one as '{}'",
                colliding_name, colliding_path, repo_name
            ));
        }
    }

    let new_repo = RepositoryConfiguration {
        name: repo_name,
        path: path_str,
        remote: detected.remote,
        branch: detected.branch.clone(),
        remote_branch: refreshed_remote_branch,
        build_strategy: None,
        build_command: String::new(),
        restart_command: None,
        out_dir: None,
    };

    config.repositories.push(new_repo);
    save_config_to_toml(&config)?;
    Ok(())
}

pub(crate) fn list() -> Result<(), ZlorbError> {
    let config = load_config_from_toml()?;
    Logger::info(format!("config: {:#?}", config));

    let mapped_repos = config.repositories.iter().map(|item| item.path.clone());
    println!("{:#?}", Vec::from_iter(mapped_repos));
    Ok(())
}

fn tracked_name_for_path(
    path: &str,
    config: &RepositoriesConfigurationFile,
) -> Option<String> {
    config
        .repositories
        .iter()
        .find(|item| same_path(&item.path, path))
        .map(|item| item.name.clone())
}

fn same_path(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (
        std::fs::canonicalize(a),
        std::fs::canonicalize(b),
    ) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn unique_repo_name(
    base: &str,
    root: &Path,
    config: &RepositoriesConfigurationFile,
) -> String {
    let taken = |name: &str| config.repositories.iter().any(|item| item.name == name);
    if !taken(base) {
        return base.to_string();
    }
    let stem = root
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|parent| parent.to_str())
        .map(|parent| format!("{}-{}", parent, base))
        .unwrap_or_else(|| base.to_string());
    if stem != base && !taken(&stem) {
        return stem;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{}-{}", stem, n);
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(exit_code: i32) -> process::ExitStatus {
        process::Command::new("sh")
            .args(["-c", &format!("exit {}", exit_code)])
            .status()
            .unwrap()
    }

    #[test]
    fn watch_succeeds_only_when_journalctl_exits_cleanly() {
        assert!(watch_with(Ok(status(0))).is_ok());
    }

    #[test]
    fn watch_fails_when_journalctl_exits_non_zero() {
        let err = format!("{:?}", watch_with(Ok(status(3))).unwrap_err());
        assert!(err.contains("journalctl exited with"), "{}", err);
        assert!(err.contains("exit status: 3"), "{}", err);
    }

    #[test]
    fn watch_propagates_a_spawn_failure() {
        let err = format!(
            "{:?}",
            watch_with(Err(ZlorbError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no journalctl",
            ))))
            .unwrap_err()
        );
        assert!(err.contains("no journalctl"), "{}", err);
    }
}


