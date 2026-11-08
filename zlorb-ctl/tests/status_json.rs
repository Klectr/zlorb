use git2::{IndexAddOption, RepositoryInitOptions, Signature};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "zlorb-ctl-status-json-test-{}-{}-{}",
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

fn run_status(config_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
        .arg("status")
        .args(args)
        .env("ZLORB_CONFIG_DIR", config_dir)
        .output()
        .unwrap()
}

fn write_config(config_dir: &Path, contents: &str) {
    std::fs::create_dir_all(config_dir).unwrap();
    std::fs::write(config_dir.join("repositories.toml"), contents).unwrap();
}

#[test]
fn status_json_reports_the_tracked_repo_and_a_daemon_boolean() {
    let root = temp_dir("repo");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    let head = init_repo(&repo_path);
    write_config(
        &config_dir,
        &format!(
            "[[repository]]\nname = \"my-app\"\npath = \"{}\"\nremote = \"origin\"\nbranch = \"main\"\nbuild_strategy = \"bun\"\n",
            repo_path.display()
        ),
    );

    let output = run_status(&config_dir, &["--json"]);
    assert!(
        output.status.success(),
        "status --json failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be one JSON document");
    assert!(value["daemon_running"].is_boolean(), "{}", value);
    assert_eq!(value["daemon_error"], serde_json::Value::Null);
    assert_eq!(value["error"], serde_json::Value::Null);
    assert_eq!(value["repositories"].as_array().unwrap().len(), 1);
    let repo = &value["repositories"][0];
    assert_eq!(repo["name"], "my-app");
    assert_eq!(repo["path"], repo_path.display().to_string().as_str());
    assert_eq!(repo["remote"], "origin");
    assert_eq!(repo["branch"], "main");
    assert_eq!(repo["head_oid"], head.as_str());
    assert_eq!(repo["build_strategy"], "bun");
    assert_eq!(repo["build_command"], "");
    assert_eq!(repo["restart_command"], serde_json::Value::Null);
    assert_eq!(repo["out_dir"], serde_json::Value::Null);
    assert_eq!(repo["out_dir_exists"], serde_json::Value::Null);
    assert_eq!(repo["remote_branch"], "main");
    assert_eq!(repo["path_exists"], true);
    assert_eq!(repo["error"], serde_json::Value::Null);
    assert_eq!(repo["last_fetch_oid"], serde_json::Value::Null);
    assert_eq!(repo["last_fetch_unix"], serde_json::Value::Null);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn status_json_with_no_config_reports_an_empty_repo_list() {
    let root = temp_dir("empty");
    let config_dir = root.join("config");
    let output = run_status(&config_dir, &["--json"]);
    assert!(
        output.status.success(),
        "status --json failed with no config: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["repositories"], serde_json::json!([]));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn status_human_output_names_the_repo_and_the_daemon_state() {
    let root = temp_dir("human");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo(&repo_path);
    write_config(
        &config_dir,
        &format!(
            "[[repository]]\nname = \"my-app\"\npath = \"{}\"\nremote = \"origin\"\nbranch = \"main\"\nbuild_strategy = \"bun\"\n",
            repo_path.display()
        ),
    );
    let output = run_status(&config_dir, &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("daemon:"), "{}", stdout);
    assert!(stdout.contains("my-app"), "{}", stdout);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn status_json_fails_on_a_malformed_config() {
    let root = temp_dir("malformed");
    let config_dir = root.join("config");
    write_config(&config_dir, "this is not = valid = toml [[\n");
    let output = run_status(&config_dir, &["--json"]);
    assert!(
        !output.status.success(),
        "status --json unexpectedly succeeded on a malformed config"
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("stdout must stay one JSON document even when the config is malformed");
    assert_eq!(value["repositories"], serde_json::json!([]));
    let error = value["error"].as_str().expect("error must be reported");
    assert!(error.contains("repositories.toml"), "{}", error);
    let _ = std::fs::remove_dir_all(&root);
}
