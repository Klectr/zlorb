use git2::{IndexAddOption, RepositoryInitOptions, Signature};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "zlorb-ctl-concurrent-test-{}-{}-{}",
        std::process::id(),
        label,
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn init_repo(path: &Path, branch: &str) {
    let mut opts = RepositoryInitOptions::new();
    opts.initial_head(branch);
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
    repo.commit(Some("HEAD"), &sig, &sig, "v1", &tree, &[])
        .unwrap();
}

fn spawn_ctl(args: &[&str], cwd: &Path, config_dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
        .args(args)
        .current_dir(cwd)
        .env("ZLORB_CONFIG_DIR", config_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn run_ctl(args: &[&str], cwd: &Path, config_dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
        .args(args)
        .current_dir(cwd)
        .env("ZLORB_CONFIG_DIR", config_dir)
        .output()
        .unwrap()
}

fn repos_file(config_dir: &Path) -> PathBuf {
    config_dir.join("repositories.toml")
}

fn count_entries(contents: &str, name: &str) -> usize {
    contents
        .lines()
        .filter(|line| *line == format!("name = \"{}\"", name))
        .count()
}

#[test]
fn concurrent_adds_of_distinct_repos_all_land() {
    let root = temp_dir("distinct-adds");
    let config_dir = root.join("config");
    let names: Vec<String> = (0..6).map(|i| format!("app-{}", i)).collect();
    for name in &names {
        init_repo(&root.join(name), "main");
    }

    let children: Vec<Child> = names
        .iter()
        .map(|name| spawn_ctl(&["add"], &root.join(name), &config_dir))
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "concurrent add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let contents = std::fs::read_to_string(repos_file(&config_dir)).unwrap();
    for name in &names {
        assert_eq!(
            count_entries(&contents, name),
            1,
            "repo {} missing or duplicated after concurrent adds:\n{}",
            name,
            contents
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn concurrent_adds_of_the_same_repo_add_it_exactly_once() {
    let root = temp_dir("same-adds");
    let config_dir = root.join("config");
    let repo_path = root.join("my-app");
    init_repo(&repo_path, "main");

    let children: Vec<Child> = (0..4)
        .map(|_| spawn_ctl(&["add"], &repo_path, &config_dir))
        .collect();
    let mut successes = 0;
    for child in children {
        let out = child.wait_with_output().unwrap();
        if out.status.success() {
            successes += 1;
        }
    }
    assert_eq!(successes, 1, "exactly one concurrent add may succeed");

    let contents = std::fs::read_to_string(repos_file(&config_dir)).unwrap();
    assert_eq!(
        count_entries(&contents, "my-app"),
        1,
        "same repo recorded more than once:\n{}",
        contents
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn concurrent_add_and_remove_do_not_lose_each_other() {
    let root = temp_dir("add-remove");
    let config_dir = root.join("config");
    let old_path = root.join("old-app");
    init_repo(&old_path, "main");
    let seeded = run_ctl(&["add"], &old_path, &config_dir);
    assert!(seeded.status.success());

    let new_names: Vec<String> = (0..4).map(|i| format!("new-{}", i)).collect();
    for name in &new_names {
        init_repo(&root.join(name), "main");
    }

    let mut children: Vec<Child> = new_names
        .iter()
        .map(|name| spawn_ctl(&["add"], &root.join(name), &config_dir))
        .collect();
    children.push(spawn_ctl(
        &["remove", "--repo-name", "old-app"],
        &root,
        &config_dir,
    ));
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "concurrent add/remove failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let contents = std::fs::read_to_string(repos_file(&config_dir)).unwrap();
    assert_eq!(
        count_entries(&contents, "old-app"),
        0,
        "removed repo came back after concurrent adds:\n{}",
        contents
    );
    for name in &new_names {
        assert_eq!(
            count_entries(&contents, name),
            1,
            "added repo {} lost to a concurrent remove:\n{}",
            name,
            contents
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
