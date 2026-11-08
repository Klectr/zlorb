use serde::Serialize;
use std::path::{Path, PathBuf};
use zlorb_lib::{
    config::{RepositoriesConfigurationFile, RepositoryConfiguration},
    error::ZlorbError,
    load_config_from_toml,
};

use crate::utils::daemon::DaemonManager;

#[derive(Debug, Serialize, PartialEq)]
pub struct StatusReport {
    pub daemon_running: bool,
    pub daemon_error: Option<String>,
    pub repositories: Vec<RepoStatus>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct RepoStatus {
    pub name: String,
    pub path: String,
    pub remote: String,
    pub branch: String,
    pub remote_branch: String,
    pub build_strategy: Option<String>,
    pub build_command: String,
    pub restart_command: Option<String>,
    pub out_dir: Option<String>,
    pub out_dir_exists: Option<bool>,
    pub path_exists: bool,
    pub head_oid: Option<String>,
    pub last_fetch_oid: Option<String>,
    pub last_fetch_unix: Option<u64>,
    pub error: Option<String>,
}

fn error_text(err: &ZlorbError) -> String {
    match err {
        ZlorbError::InvalidConfig(message)
        | ZlorbError::Other(message)
        | ZlorbError::ConfigParseError(message)
        | ZlorbError::PermissionDenied(message)
        | ZlorbError::SerializationErrorGeneric(message) => message.clone(),
        ZlorbError::Git(inner) => inner.to_string(),
        ZlorbError::Io(inner) => inner.to_string(),
        other => format!("{:?}", other),
    }
}

fn first_error(current: &mut Option<String>, candidate: String) {
    if current.is_none() {
        *current = Some(candidate);
    }
}

fn is_oid_token(token: &str) -> bool {
    (token.len() == 40 || token.len() == 64)
        && token.chars().all(|c| c.is_ascii_hexdigit())
}

fn fetched_oid_for_branch(contents: &str, remote_branch: &str) -> Option<String> {
    let branch_marker = format!("branch '{}'", remote_branch);
    for line in contents.lines() {
        if line.contains("not-for-merge") || !line.contains(&branch_marker) {
            continue;
        }
        let token = line.split_whitespace().next()?;
        if is_oid_token(token) {
            return Some(token.to_string());
        }
    }
    None
}

pub fn probe_repo(config: &RepositoryConfiguration) -> RepoStatus {
    let mut error: Option<String> = None;
    if let Err(e) = config.validate_branch_names() {
        first_error(&mut error, error_text(&e));
    }
    if let Err(e) = config.validate_remote_name() {
        first_error(&mut error, error_text(&e));
    }
    let build_strategy = match config.resolved_build_strategy() {
        Ok(kind) => Some(kind.name().to_string()),
        Err(e) => {
            first_error(&mut error, error_text(&e));
            None
        }
    };

    let path = Path::new(&config.path);
    let path_exists = path.exists();
    let mut head_oid = None;
    let mut last_fetch_oid = None;
    let mut last_fetch_unix = None;
    let mut out_dir_exists = None;

    if !path_exists {
        first_error(
            &mut error,
            format!("path {} does not exist", config.path),
        );
    } else {
        if let Some(out_dir) = &config.out_dir {
            let out_path = PathBuf::from(out_dir);
            let full = if out_path.is_absolute() {
                out_path
            } else {
                path.join(out_path)
            };
            out_dir_exists = Some(full.exists());
        }
        match git2::Repository::open(path) {
            Ok(repo) => {
                match repo.head() {
                    Ok(head) => match head.target() {
                        Some(oid) => head_oid = Some(oid.to_string()),
                        None => first_error(
                            &mut error,
                            format!("HEAD of {} has no target", config.path),
                        ),
                    },
                    Err(e) => first_error(
                        &mut error,
                        format!("could not read HEAD of {}: {}", config.path, e),
                    ),
                }
                if let Err(e) = repo.find_remote(&config.remote) {
                    first_error(
                        &mut error,
                        format!(
                            "remote '{}' could not be read in {}: {}",
                            config.remote, config.path, e
                        ),
                    );
                }
                let fetch_head = repo.commondir().join("FETCH_HEAD");
                if let Ok(contents) = std::fs::read_to_string(&fetch_head) {
                    last_fetch_oid = fetched_oid_for_branch(
                        &contents,
                        config.resolved_remote_branch(),
                    );
                }
                if let Ok(metadata) = std::fs::metadata(&fetch_head) {
                    last_fetch_unix = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_secs());
                }
            }
            Err(e) => {
                first_error(
                    &mut error,
                    format!("could not open {} as a git repository: {}", config.path, e),
                );
            }
        }
    }

    RepoStatus {
        name: config.name.clone(),
        path: config.path.clone(),
        remote: config.remote.clone(),
        branch: config.branch.clone(),
        remote_branch: config.resolved_remote_branch().to_string(),
        build_strategy,
        build_command: config.build_command.clone(),
        restart_command: config.resolved_restart_command().map(|c| c.to_string()),
        out_dir: config.out_dir.clone(),
        out_dir_exists,
        path_exists,
        head_oid,
        last_fetch_oid,
        last_fetch_unix,
        error,
    }
}

pub fn build_report(
    config: &RepositoriesConfigurationFile,
    daemon: Result<bool, ZlorbError>,
) -> StatusReport {
    let (daemon_running, daemon_error) = match daemon {
        Ok(running) => (running, None),
        Err(e) => (false, Some(error_text(&e))),
    };
    StatusReport {
        daemon_running,
        daemon_error,
        repositories: config.repositories.iter().map(probe_repo).collect(),
        error: None,
    }
}

fn error_report(message: String) -> StatusReport {
    StatusReport {
        daemon_running: false,
        daemon_error: None,
        repositories: Vec::new(),
        error: Some(message),
    }
}

fn print_json_report(report: &StatusReport) {
    match serde_json::to_string_pretty(report) {
        Ok(rendered) => println!("{}", rendered),
        Err(e) => {
            eprintln!("could not serialize the status report: {}", e);
            std::process::exit(1);
        }
    }
}

pub(crate) fn status(json: bool) -> Result<(), ZlorbError> {
    let config = match load_config_from_toml() {
        Ok(config) => config,
        Err(e) if json => {
            print_json_report(&error_report(error_text(&e)));
            std::process::exit(1);
        }
        Err(e) => return Err(e),
    };
    let report = build_report(&config, DaemonManager::is_running());
    if json {
        print_json_report(&report);
        return Ok(());
    }
    println!(
        "daemon: {}",
        if report.daemon_running {
            "running"
        } else {
            "not running"
        }
    );
    if let Some(err) = &report.daemon_error {
        println!("daemon check failed: {}", err);
    }
    if report.repositories.is_empty() {
        println!("no repositories tracked");
        return Ok(());
    }
    for repo in &report.repositories {
        println!("{}: {}", repo.name, repo.path);
        println!(
            "  branch: {} (remote {} tracks {})",
            repo.branch, repo.remote, repo.remote_branch
        );
        match &repo.build_strategy {
            Some(strategy) => println!("  build_strategy: {}", strategy),
            None => println!("  build_strategy: (unresolved)"),
        }
        match &repo.head_oid {
            Some(oid) => println!("  head: {}", oid),
            None => println!("  head: (unknown)"),
        }
        match (&repo.last_fetch_oid, repo.last_fetch_unix) {
            (Some(oid), Some(unix)) => println!("  last_fetch: {} at {}", oid, unix),
            (Some(oid), None) => println!("  last_fetch: {}", oid),
            (None, Some(unix)) => {
                println!("  last_fetch: (no line for the tracked branch) at {}", unix)
            }
            (None, None) => println!("  last_fetch: (never)"),
        }
        if let Some(err) = &repo.error {
            println!("  error: {}", err);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{IndexAddOption, RepositoryInitOptions, Signature};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "zlorb-ctl-status-test-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_repo(path: &Path) -> String {
        let mut opts = RepositoryInitOptions::new();
        opts.initial_head("main");
        let repo = git2::Repository::init_opts(path, &opts).unwrap();
        std::fs::write(path.join("file.txt"), "v1").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("zlorb-test", "zlorb-test@example.com").unwrap();
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, "v1", &tree, &[])
            .unwrap();
        repo.remote("origin", "https://example.com/repo.git")
            .unwrap();
        oid.to_string()
    }

    fn config_for(path: &Path, strategy: Option<&str>) -> RepositoryConfiguration {
        RepositoryConfiguration {
            name: "demo".to_string(),
            path: path.to_string_lossy().into_owned(),
            remote: "origin".to_string(),
            branch: "main".to_string(),
            remote_branch: None,
            build_strategy: strategy.map(|s| s.to_string()),
            build_command: String::new(),
            restart_command: None,
            out_dir: None,
        }
    }

    #[test]
    fn probe_of_a_healthy_repo_reports_head_strategy_and_no_error() {
        let root = temp_dir("healthy");
        let repo_path = root.join("demo");
        let head = init_repo(&repo_path);
        let status = probe_repo(&config_for(&repo_path, Some("bun")));
        assert!(status.path_exists);
        assert_eq!(status.head_oid.as_deref(), Some(head.as_str()));
        assert_eq!(status.build_strategy.as_deref(), Some("bun"));
        assert_eq!(status.remote_branch, "main");
        assert_eq!(status.error, None);
        assert_eq!(status.last_fetch_oid, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_reads_last_fetch_from_fetch_head() {
        let root = temp_dir("fetch-head");
        let repo_path = root.join("demo");
        let head = init_repo(&repo_path);
        std::fs::write(
            repo_path.join(".git").join("FETCH_HEAD"),
            format!("{}\t\tbranch 'main' of https://example.com/repo\n", head),
        )
        .unwrap();
        let status = probe_repo(&config_for(&repo_path, Some("rust")));
        assert_eq!(status.last_fetch_oid.as_deref(), Some(head.as_str()));
        assert!(status.last_fetch_unix.is_some_and(|unix| unix > 0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_of_a_missing_path_reports_it_as_the_error() {
        let root = temp_dir("missing");
        let missing = root.join("nope");
        let status = probe_repo(&config_for(&missing, Some("bun")));
        assert!(!status.path_exists);
        assert_eq!(status.head_oid, None);
        let err = status.error.expect("missing path must error");
        assert!(err.contains("does not exist"), "{}", err);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_of_an_unknown_strategy_reports_it_and_no_strategy() {
        let root = temp_dir("strategy");
        let repo_path = root.join("demo");
        init_repo(&repo_path);
        let status = probe_repo(&config_for(&repo_path, Some("webpack")));
        assert_eq!(status.build_strategy, None);
        let err = status.error.expect("unknown strategy must error");
        assert!(err.contains("webpack"), "{}", err);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_of_a_missing_remote_reports_it() {
        let root = temp_dir("remote");
        let repo_path = root.join("demo");
        init_repo(&repo_path);
        let mut config = config_for(&repo_path, Some("bun"));
        config.remote = "upstream".to_string();
        let status = probe_repo(&config);
        let err = status.error.expect("missing remote must error");
        assert!(err.contains("upstream"), "{}", err);
        assert!(err.contains("could not be read"), "{}", err);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_picks_the_tracked_branch_line_out_of_a_multi_ref_fetch_head() {
        let root = temp_dir("fetch-multi");
        let repo_path = root.join("demo");
        let head = init_repo(&repo_path);
        let other = "a".repeat(40);
        std::fs::write(
            repo_path.join(".git").join("FETCH_HEAD"),
            format!(
                "{}\t\tbranch 'other' of https://example.com/repo\n{}\t\tbranch 'main' of https://example.com/repo\n{}\t\tnot-for-merge\tbranch 'main' of https://example.com/repo\n",
                other, head, other
            ),
        )
        .unwrap();
        let status = probe_repo(&config_for(&repo_path, Some("bun")));
        assert_eq!(status.last_fetch_oid.as_deref(), Some(head.as_str()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_of_an_unborn_head_reports_an_error_instead_of_a_healthy_repo() {
        let root = temp_dir("unborn");
        let repo_path = root.join("demo");
        let mut opts = RepositoryInitOptions::new();
        opts.initial_head("main");
        let repo = git2::Repository::init_opts(&repo_path, &opts).unwrap();
        repo.remote("origin", "https://example.com/repo.git")
            .unwrap();
        drop(repo);
        let status = probe_repo(&config_for(&repo_path, Some("bun")));
        assert_eq!(status.head_oid, None);
        let err = status.error.expect("unborn HEAD must error");
        assert!(err.contains("HEAD"), "{}", err);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_of_a_linked_worktree_reads_fetch_head_from_the_common_dir() {
        let root = temp_dir("worktree");
        let repo_path = root.join("demo");
        let head = init_repo(&repo_path);
        let repo = git2::Repository::open(&repo_path).unwrap();
        let worktree_path = root.join("demo-wt");
        repo.worktree("demo-wt", &worktree_path, None).unwrap();
        drop(repo);
        std::fs::write(
            repo_path.join(".git").join("FETCH_HEAD"),
            format!("{}\t\tbranch 'main' of https://example.com/repo\n", head),
        )
        .unwrap();
        let status = probe_repo(&config_for(&worktree_path, Some("bun")));
        assert_eq!(status.head_oid.as_deref(), Some(head.as_str()));
        assert_eq!(status.last_fetch_oid.as_deref(), Some(head.as_str()));
        assert_eq!(status.error, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn report_serializes_with_the_documented_field_names() {
        let config = RepositoriesConfigurationFile {
            repositories: vec![],
        };
        let report = build_report(&config, Ok(true));
        let value: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["daemon_running"], serde_json::json!(true));
        assert_eq!(value["daemon_error"], serde_json::Value::Null);
        assert_eq!(value["error"], serde_json::Value::Null);
        assert_eq!(value["repositories"], serde_json::json!([]));

        let failed = build_report(&config, Err(ZlorbError::Other("no systemctl".to_string())));
        assert!(!failed.daemon_running);
        assert_eq!(failed.daemon_error.as_deref(), Some("no systemctl"));
    }

    #[test]
    fn repo_status_serializes_with_every_documented_field_name() {
        let root = temp_dir("field-names");
        let repo_path = root.join("demo");
        init_repo(&repo_path);
        let status = probe_repo(&config_for(&repo_path, Some("bun")));
        let value: serde_json::Value = serde_json::to_value(&status).unwrap();
        let object = value.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(|key| key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "branch",
                "build_command",
                "build_strategy",
                "error",
                "head_oid",
                "last_fetch_oid",
                "last_fetch_unix",
                "name",
                "out_dir",
                "out_dir_exists",
                "path",
                "path_exists",
                "remote",
                "remote_branch",
                "restart_command",
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn out_dir_existence_is_reported_relative_to_the_repo() {
        let root = temp_dir("out-dir");
        let repo_path = root.join("demo");
        init_repo(&repo_path);
        let mut config = config_for(&repo_path, Some("bun"));
        config.out_dir = Some("dist".to_string());
        assert_eq!(probe_repo(&config).out_dir_exists, Some(false));
        std::fs::create_dir_all(repo_path.join("dist")).unwrap();
        assert_eq!(probe_repo(&config).out_dir_exists, Some(true));
        config.out_dir = None;
        assert_eq!(probe_repo(&config).out_dir_exists, None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
