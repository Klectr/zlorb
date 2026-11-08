use std::{env, process};
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

    throw_if_repo_exists(&repo_name, &config)?;

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

fn throw_if_repo_exists(
    repo_name: &String,
    config: &RepositoriesConfigurationFile,
) -> Result<(), ZlorbError> {
    if let Some(found_el) = config
        .repositories
        .iter()
        .find(|item| &item.name == repo_name)
    {
        let err = format!("{} is already in the configuration", found_el.name);
        return Err(ZlorbError::Other(err));
    }

    Ok(())
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
