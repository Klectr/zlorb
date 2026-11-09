#![cfg(unix)]

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "zlorb-ctl-noninteractive-test-{}-{}-{}",
        std::process::id(),
        label,
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let script = dir.join(name);
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn run_ctl(config_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
        .args(args)
        .env("ZLORB_CONFIG_DIR", config_dir)
        .output()
        .unwrap()
}

fn assert_one_error_line(output: &Output) -> String {
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        !stdout.contains("error:"),
        "error leaked to stdout: {}",
        stdout
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!stderr.contains('\x1b'), "ANSI escape in stderr: {:?}", stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 1, "stderr must be one line: {:?}", stderr);
    assert!(
        lines[0].starts_with("error: "),
        "error line must start with 'error: ': {}",
        lines[0]
    );
    lines[0].to_string()
}

#[test]
fn a_malformed_config_fails_with_one_error_line_on_stderr_only() {
    let root = temp_dir("malformed");
    let config_dir = root.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("repositories.toml"),
        "this is not = valid = toml [[\n",
    )
    .unwrap();
    for args in [&["list"][..], &["status"][..], &["remove", "--repo-name", "x"][..]] {
        let output = run_ctl(&config_dir, args);
        let line = assert_one_error_line(&output);
        assert!(line.contains("Failed to parse config"), "{}", line);
        assert!(line.contains("repositories.toml"), "{}", line);
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn add_outside_a_repo_fails_with_one_error_line_on_stderr_only() {
    let root = temp_dir("add-norepo");
    let config_dir = root.join("config");
    let output = Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
        .arg("add")
        .env("ZLORB_CONFIG_DIR", &config_dir)
        .current_dir(&root)
        .output()
        .unwrap();
    assert_one_error_line(&output);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn clap_errors_go_to_stderr_with_a_non_zero_exit_and_empty_stdout() {
    let root = temp_dir("clap");
    let config_dir = root.join("config");
    for args in [&["remove"][..], &["bogus"][..], &["status", "--nope"][..]] {
        let output = run_ctl(&config_dir, args);
        assert!(!output.status.success(), "{:?} unexpectedly succeeded", args);
        assert!(
            output.stdout.is_empty(),
            "{:?} wrote to stdout: {}",
            args,
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            !output.stderr.is_empty(),
            "{:?} wrote nothing to stderr",
            args
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains('\x1b'),
            "{:?} emitted ANSI escapes",
            args
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn watch_fails_when_journalctl_exits_non_zero() {
    let root = temp_dir("watch-fails");
    let bin_dir = root.join("bin");
    write_script(&bin_dir, "journalctl", "#!/bin/sh\nexit 3\n");
    let output = Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
        .arg("watch")
        .env("ZLORB_CONFIG_DIR", root.join("config"))
        .env("PATH", &bin_dir)
        .output()
        .unwrap();
    let line = assert_one_error_line(&output);
    assert!(line.contains("journalctl exited with"), "{}", line);
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("End: Fail"),
        "watch still reports failure as a normal end"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn watch_fails_when_journalctl_is_missing() {
    let root = temp_dir("watch-missing");
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
        .arg("watch")
        .env("ZLORB_CONFIG_DIR", root.join("config"))
        .env("PATH", &bin_dir)
        .output()
        .unwrap();
    assert_one_error_line(&output);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn watch_succeeds_when_journalctl_exits_cleanly() {
    let root = temp_dir("watch-ok");
    let bin_dir = root.join("bin");
    write_script(&bin_dir, "journalctl", "#!/bin/sh\nexit 0\n");
    let output = Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
        .arg("watch")
        .env("ZLORB_CONFIG_DIR", root.join("config"))
        .env("PATH", &bin_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "watch failed on a clean journalctl exit: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}

const FAKE_SYSTEMCTL_FAILING_START: &str = r#"#!/bin/sh
if [ "$1" = "--user" ]; then
    shift
fi
case "$1" in
    is-active)
        printf 'inactive\n'
        exit 3
        ;;
    start)
        exit 1
        ;;
    *)
        exit 1
        ;;
esac
"#;

#[test]
fn a_failed_daemon_start_reports_its_error_on_stderr_only() {
    let root = temp_dir("start-fails");
    let bin_dir = root.join("bin");
    write_script(&bin_dir, "systemctl", FAKE_SYSTEMCTL_FAILING_START);
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
        .arg("start")
        .env("ZLORB_CONFIG_DIR", root.join("config"))
        .env("PATH", path)
        .output()
        .unwrap();
    let line = assert_one_error_line(&output);
    assert!(line.contains("start zlorb failed"), "{}", line);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("Starting the daemon"), "{}", stdout);
    assert!(!stdout.contains('\x1b'), "ANSI escape in stdout: {:?}", stdout);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn web_on_an_occupied_port_fails_with_an_error_line_instead_of_panicking() {
    let root = temp_dir("web-occupied");
    let held = std::net::TcpListener::bind("127.0.0.1:3000")
        .expect("port 3000 must be free to hold for this test");
    let mut child = Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
        .arg("web")
        .env("ZLORB_CONFIG_DIR", root.join("config"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("web did not exit on an occupied port");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    drop(held);
    let mut stdout = String::new();
    let mut stderr = String::new();
    child.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert!(!status.success(), "web succeeded on an occupied port");
    assert_eq!(
        status.code(),
        Some(1),
        "web must exit 1, not panic (101): stdout={} stderr={}",
        stdout,
        stderr
    );
    assert!(!stderr.contains("panicked"), "web panicked: {}", stderr);
    assert!(!stdout.contains("error:"), "error leaked to stdout: {}", stdout);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 1, "stderr must be one line: {:?}", stderr);
    assert!(lines[0].starts_with("error: "), "{}", lines[0]);
    let _ = std::fs::remove_dir_all(&root);
}
