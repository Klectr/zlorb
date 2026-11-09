#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$ZLORB_FAKE_SYSTEMCTL_LOG"
if [ "$1" = "--user" ]; then
    shift
fi
case "$1" in
    is-active)
        if [ -f "$ZLORB_FAKE_SYSTEMCTL_BROKEN" ]; then
            IFS= read -r mode < "$ZLORB_FAKE_SYSTEMCTL_BROKEN"
            if [ "$mode" = "plausible" ]; then
                IFS= read -r state < "$ZLORB_FAKE_SYSTEMCTL_STATE"
                printf '%s\n' "$state"
            fi
            echo "bus exploded" >&2
            exit 1
        fi
        IFS= read -r state < "$ZLORB_FAKE_SYSTEMCTL_STATE"
        printf '%s\n' "$state"
        if [ "$state" = "active" ]; then
            exit 0
        fi
        exit 3
        ;;
    start|restart)
        if [ -f "$ZLORB_FAKE_SYSTEMCTL_FAIL" ]; then
            exit 1
        fi
        printf 'active\n' > "$ZLORB_FAKE_SYSTEMCTL_STATE"
        exit 0
        ;;
    stop)
        if [ -f "$ZLORB_FAKE_SYSTEMCTL_FAIL" ]; then
            exit 1
        fi
        printf 'inactive\n' > "$ZLORB_FAKE_SYSTEMCTL_STATE"
        exit 0
        ;;
    *)
        exit 1
        ;;
esac
"#;

struct FakeSystemctl {
    root: PathBuf,
    bin_dir: PathBuf,
    log: PathBuf,
    state: PathBuf,
    broken: PathBuf,
    fail: PathBuf,
}

fn temp_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "zlorb-ctl-daemon-systemctl-test-{}-{}-{}",
        std::process::id(),
        label,
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn running_as_root() -> bool {
    let output = Command::new("id").arg("-u").output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim() == "0"
}

fn scope() -> &'static str {
    if running_as_root() {
        ""
    } else {
        "--user "
    }
}

fn argv(verb: &str) -> String {
    format!("{}{} zlorb", scope(), verb)
}

impl FakeSystemctl {
    fn new(label: &str, state: &str) -> Self {
        let root = temp_dir(label);
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let script = bin_dir.join("systemctl");
        std::fs::write(&script, FAKE_SYSTEMCTL).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let state_path = root.join("state");
        std::fs::write(&state_path, format!("{}\n", state)).unwrap();
        Self {
            log: root.join("argv.log"),
            state: state_path,
            broken: root.join("broken"),
            fail: root.join("fail"),
            bin_dir,
            root,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
            .args(args)
            .env("PATH", &self.bin_dir)
            .env("ZLORB_FAKE_SYSTEMCTL_LOG", &self.log)
            .env("ZLORB_FAKE_SYSTEMCTL_STATE", &self.state)
            .env("ZLORB_FAKE_SYSTEMCTL_BROKEN", &self.broken)
            .env("ZLORB_FAKE_SYSTEMCTL_FAIL", &self.fail)
            .env("ZLORB_CONFIG_DIR", self.root.join("config"))
            .output()
            .unwrap()
    }

    fn logged(&self) -> Vec<String> {
        match std::fs::read_to_string(&self.log) {
            Ok(contents) => contents.lines().map(|line| line.to_string()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn break_read(&self, mode: &str) {
        std::fs::write(&self.broken, mode).unwrap();
    }

    fn current_state(&self) -> String {
        std::fs::read_to_string(&self.state).unwrap().trim().to_string()
    }
}

impl Drop for FakeSystemctl {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn assert_success(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{} failed: {}{}",
        what,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn stop_on_an_active_daemon_probes_then_issues_only_the_stop_verb() {
    let fake = FakeSystemctl::new("stop-active", "active");
    let output = fake.run(&["stop"]);
    assert_success(&output, "stop");
    assert_eq!(fake.logged(), vec![argv("is-active"), argv("stop")]);
    assert_eq!(fake.current_state(), "inactive");
}

#[test]
fn stop_on_an_inactive_daemon_issues_no_verb() {
    let fake = FakeSystemctl::new("stop-inactive", "inactive");
    let output = fake.run(&["stop"]);
    assert_success(&output, "stop");
    assert_eq!(fake.logged(), vec![argv("is-active")]);
    assert_eq!(fake.current_state(), "inactive");
}

#[test]
fn stop_on_a_failed_daemon_still_issues_the_stop_verb() {
    let fake = FakeSystemctl::new("stop-failed", "failed");
    let output = fake.run(&["stop"]);
    assert_success(&output, "stop");
    assert_eq!(fake.logged(), vec![argv("is-active"), argv("stop")]);
}

#[test]
fn start_on_an_inactive_daemon_probes_then_issues_only_the_start_verb() {
    let fake = FakeSystemctl::new("start-inactive", "inactive");
    let output = fake.run(&["start"]);
    assert_success(&output, "start");
    assert_eq!(fake.logged(), vec![argv("is-active"), argv("start")]);
    assert_eq!(fake.current_state(), "active");
}

#[test]
fn start_on_a_running_daemon_issues_no_verb_and_never_restarts() {
    let fake = FakeSystemctl::new("start-active", "active");
    let output = fake.run(&["start"]);
    assert_success(&output, "start");
    assert_eq!(fake.logged(), vec![argv("is-active")]);
    assert_eq!(fake.current_state(), "active");
}

#[test]
fn restart_on_a_stopped_daemon_issues_restart_without_probing() {
    let fake = FakeSystemctl::new("restart-inactive", "inactive");
    let output = fake.run(&["restart"]);
    assert_success(&output, "restart");
    assert_eq!(fake.logged(), vec![argv("restart")]);
    assert_eq!(fake.current_state(), "active");
}

#[test]
fn restart_on_a_running_daemon_issues_restart_without_probing() {
    let fake = FakeSystemctl::new("restart-active", "active");
    let output = fake.run(&["restart"]);
    assert_success(&output, "restart");
    assert_eq!(fake.logged(), vec![argv("restart")]);
}

#[test]
fn systemctl_argv_carries_the_scope_for_the_effective_user() {
    let fake = FakeSystemctl::new("scope", "inactive");
    let output = fake.run(&["start"]);
    assert_success(&output, "start");
    for line in fake.logged() {
        if running_as_root() {
            assert!(
                !line.contains("--user"),
                "root argv must not carry --user: {:?}",
                line
            );
        } else {
            assert!(
                line.starts_with("--user "),
                "non-root argv must carry --user: {:?}",
                line
            );
        }
    }
    assert_eq!(fake.logged(), vec![argv("is-active"), argv("start")]);
}

#[test]
fn a_failing_stop_verb_makes_the_cli_exit_non_zero() {
    let fake = FakeSystemctl::new("stop-verb-fails", "active");
    std::fs::write(&fake.fail, "").unwrap();
    let output = fake.run(&["stop"]);
    assert!(!output.status.success(), "stop succeeded on a failing verb");
    assert_eq!(fake.logged(), vec![argv("is-active"), argv("stop")]);
}

#[test]
fn a_failing_start_verb_makes_the_cli_exit_non_zero() {
    let fake = FakeSystemctl::new("start-verb-fails", "inactive");
    std::fs::write(&fake.fail, "").unwrap();
    let output = fake.run(&["start"]);
    assert!(!output.status.success(), "start succeeded on a failing verb");
    assert_eq!(fake.logged(), vec![argv("is-active"), argv("start")]);
}

#[test]
fn a_failing_restart_verb_makes_the_cli_exit_non_zero() {
    let fake = FakeSystemctl::new("restart-verb-fails", "active");
    std::fs::write(&fake.fail, "").unwrap();
    let output = fake.run(&["restart"]);
    assert!(
        !output.status.success(),
        "restart succeeded on a failing verb"
    );
    assert_eq!(fake.logged(), vec![argv("restart")]);
}

#[test]
fn an_inconclusive_state_read_blocks_stop_without_issuing_a_verb() {
    let fake = FakeSystemctl::new("stop-broken-state", "active");
    std::fs::write(&fake.broken, "").unwrap();
    let output = fake.run(&["stop"]);
    assert!(
        !output.status.success(),
        "stop no-opped or succeeded on an inconclusive state read"
    );
    assert_eq!(fake.logged(), vec![argv("is-active")]);
    assert_eq!(fake.current_state(), "active");
}

#[test]
fn an_inconclusive_state_read_blocks_start_without_issuing_a_verb() {
    let fake = FakeSystemctl::new("start-broken-state", "inactive");
    std::fs::write(&fake.broken, "").unwrap();
    let output = fake.run(&["start"]);
    assert!(
        !output.status.success(),
        "start succeeded on an inconclusive state read"
    );
    assert_eq!(fake.logged(), vec![argv("is-active")]);
    assert_eq!(fake.current_state(), "inactive");
}

#[test]
fn start_on_a_failed_daemon_issues_the_start_verb() {
    let fake = FakeSystemctl::new("start-failed", "failed");
    let output = fake.run(&["start"]);
    assert_success(&output, "start");
    assert_eq!(fake.logged(), vec![argv("is-active"), argv("start")]);
}

#[test]
fn stop_on_an_activating_daemon_issues_the_stop_verb() {
    let fake = FakeSystemctl::new("stop-activating", "activating");
    let output = fake.run(&["stop"]);
    assert_success(&output, "stop");
    assert_eq!(fake.logged(), vec![argv("is-active"), argv("stop")]);
}

#[test]
fn a_plausible_state_with_a_bad_exit_blocks_stop_without_issuing_a_verb() {
    let fake = FakeSystemctl::new("stop-plausible-broken", "inactive");
    fake.break_read("plausible");
    let output = fake.run(&["stop"]);
    assert!(
        !output.status.success(),
        "stop trusted a state printed by a failed is-active query"
    );
    assert_eq!(fake.logged(), vec![argv("is-active")]);
}

#[test]
fn a_plausible_state_with_a_bad_exit_blocks_start_without_issuing_a_verb() {
    let fake = FakeSystemctl::new("start-plausible-broken", "active");
    fake.break_read("plausible");
    let output = fake.run(&["start"]);
    assert!(
        !output.status.success(),
        "start trusted a state printed by a failed is-active query"
    );
    assert_eq!(fake.logged(), vec![argv("is-active")]);
}

#[test]
fn status_on_an_inconclusive_read_reports_the_daemon_check_failure() {
    let fake = FakeSystemctl::new("status-broken", "active");
    fake.break_read("");
    let output = fake.run(&["status"]);
    assert_success(&output, "status");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("daemon: not running"), "{}", stdout);
    assert!(stdout.contains("daemon check failed"), "{}", stdout);
    assert_eq!(fake.logged(), vec![argv("is-active")]);
}

#[test]
fn status_reports_running_when_the_fake_reports_active() {
    let fake = FakeSystemctl::new("status-active", "active");
    let output = fake.run(&["status"]);
    assert_success(&output, "status");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("daemon: running"), "{}", stdout);
    assert_eq!(fake.logged(), vec![argv("is-active")]);
}

#[test]
fn status_reports_not_running_when_the_fake_reports_inactive() {
    let fake = FakeSystemctl::new("status-inactive", "inactive");
    let output = fake.run(&["status"]);
    assert_success(&output, "status");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("daemon: not running"), "{}", stdout);
    assert_eq!(fake.logged(), vec![argv("is-active")]);
}
