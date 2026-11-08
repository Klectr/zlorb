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

    let out = cmd.status().map_err(ZlorbError::Io);
    match out {
        Ok(status) => {
            if status.success() {
                println!("End");
                Ok(())
            } else {
                println!("End: Fail");
                Ok(())
            }
        }
        Err(e) => {
            e.print();
            Err(e)
        }
    }
}

pub(crate) fn remove(repo_name: String) -> Result<(), ZlorbError> {
    let _config_lock = lock_repo_config()?;
    let mut config = load_config_from_toml()?;
    throw_if_repo_not_found(&repo_name, &config)?;

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
    let detected = detect_repo(&cwd)?;
    let path_str = detected.root.to_string_lossy().to_string();

    let repo_name = detected
        .root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .ok_or_else(|| {
            ZlorbError::Other(format!(
                "could not read a repository name from {}",
                path_str
            ))
        })?;

    let _config_lock = lock_repo_config()?;
    let mut config = load_config_from_toml()?;

    throw_if_path_tracked(&path_str, &config)?;

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
        remote_branch: if detected.remote_branch != detected.branch {
            Some(detected.remote_branch)
        } else {
            None
        },
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

fn throw_if_path_tracked(
    path: &str,
    config: &RepositoriesConfigurationFile,
) -> Result<(), ZlorbError> {
    if let Some(found) = config
        .repositories
        .iter()
        .find(|item| same_path(&item.path, path))
    {
        let err = format!(
            "{} is already in the configuration as '{}'",
            path, found.name
        );
        return Err(ZlorbError::Other(err));
    }

    Ok(())
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
        .map(|parent| format!("{}-{}", parent.to_string_lossy(), base))
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

fn throw_if_repo_not_found(
    repo_name: &String,
    config: &RepositoriesConfigurationFile,
) -> Result<(), ZlorbError> {
    if config
        .repositories
        .iter()
        .find(|repo| &repo.name == repo_name)
        .is_none()
    {
        let err = format!("repo '{}' not found in tracked repos", repo_name);
        return Err(ZlorbError::Other(err));
    };

    Ok(())
}
