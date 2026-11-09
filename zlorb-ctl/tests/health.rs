#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static COUNTER: AtomicU64 = AtomicU64::new(0);
static PORT_LOCK: Mutex<()> = Mutex::new(());

const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
if [ "$1" = "--user" ]; then
    shift
fi
case "$1" in
    is-active)
        IFS= read -r state < "$ZLORB_FAKE_SYSTEMCTL_STATE"
        printf '%s\n' "$state"
        if [ "$state" = "active" ]; then
            exit 0
        fi
        exit 3
        ;;
    *)
        exit 1
        ;;
esac
"#;

fn temp_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "zlorb-ctl-health-test-{}-{}-{}",
        std::process::id(),
        label,
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct FakeDaemon {
    root: PathBuf,
    bin_dir: PathBuf,
    state: PathBuf,
}

impl FakeDaemon {
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
            root,
            bin_dir,
            state: state_path,
        }
    }

    fn run_health(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_zlorb-ctl"))
            .arg("health")
            .args(args)
            .env("PATH", &self.bin_dir)
            .env("ZLORB_FAKE_SYSTEMCTL_STATE", &self.state)
            .env("ZLORB_CONFIG_DIR", self.root.join("config"))
            .output()
            .unwrap()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct WebServer {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

fn port_lock() -> std::sync::MutexGuard<'static, ()> {
    PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn start_web_server(status_line: &'static str) -> WebServer {
    let listener = TcpListener::bind("127.0.0.1:3000")
        .expect("port 3000 must be free to stand in for the web UI in this test");
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let mut buf = [0u8; 512];
                    let _ = stream.read(&mut buf);
                    let request = String::from_utf8_lossy(&buf);
                    if !request.starts_with("GET /healthz ") {
                        let _ = stream.write_all(
                            b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                        );
                        continue;
                    }
                    let body = if status_line.contains(" 200 ") {
                        "zlorb-web ok\n"
                    } else {
                        ""
                    };
                    let _ = stream.write_all(
                        format!(
                            "{}\r\nContent-Length: {}\r\n\r\n{}",
                            status_line,
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    WebServer {
        stop,
        handle: Some(handle),
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn assert_one_error_line(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!stderr.contains('\x1b'), "ANSI escape in stderr: {:?}", stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 1, "stderr must be one line: {:?}", stderr);
    assert!(lines[0].starts_with("error: "), "{}", lines[0]);
    lines[0].to_string()
}

#[test]
fn health_succeeds_when_the_daemon_is_active_and_the_web_answers_200() {
    let _guard = port_lock();
    let daemon = FakeDaemon::new("healthy", "active");
    let _web = start_web_server("HTTP/1.0 200 OK");

    let text = daemon.run_health(&[]);
    assert!(
        text.status.success(),
        "health failed on a healthy setup: {}{}",
        String::from_utf8_lossy(&text.stdout),
        String::from_utf8_lossy(&text.stderr)
    );
    let stdout = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(stdout.contains("daemon: running"), "{}", stdout);
    assert!(stdout.contains("web: responding (HTTP 200)"), "{}", stdout);
    assert!(stdout.contains("healthy: yes"), "{}", stdout);
    assert!(text.stderr.is_empty(), "{}", String::from_utf8_lossy(&text.stderr));

    let json = daemon.run_health(&["--json"]);
    assert!(json.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("stdout must be one JSON document");
    assert_eq!(value["daemon_running"], true);
    assert_eq!(value["daemon_error"], serde_json::Value::Null);
    assert_eq!(value["web_responding"], true);
    assert_eq!(value["web_status"], 200);
    assert_eq!(value["web_error"], serde_json::Value::Null);
    assert_eq!(value["healthy"], true);
}

#[test]
fn health_fails_when_a_foreign_server_squats_on_the_web_port() {
    let _guard = port_lock();
    let daemon = FakeDaemon::new("web-squatted", "active");
    let listener = TcpListener::bind("127.0.0.1:3000")
        .expect("port 3000 must be free to stand in for a squatter in this test");
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        while !thread_stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let mut buf = [0u8; 512];
                    let _ = stream.read(&mut buf);
                    let _ = stream.write_all(
                        b"HTTP/1.0 200 OK\r\nContent-Length: 11\r\n\r\nhello world",
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    let squatter = WebServer {
        stop,
        handle: Some(handle),
    };

    let output = daemon.run_health(&["--json"]);
    drop(squatter);
    assert!(
        !output.status.success(),
        "health succeeded on a foreign server squatting on port 3000"
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be one JSON document");
    assert_eq!(value["web_responding"], false);
    assert_eq!(value["healthy"], false);
    assert!(
        value["web_error"]
            .as_str()
            .expect("web_error must be reported")
            .contains("did not identify"),
        "{}",
        value
    );
}

#[test]
fn health_fails_when_the_web_is_not_listening() {
    let _guard = port_lock();
    let probe = TcpListener::bind("127.0.0.1:3000").expect(
        "port 3000 must actually be free for the web-down case; something is already listening on it",
    );
    drop(probe);
    let daemon = FakeDaemon::new("web-down", "active");

    let output = daemon.run_health(&["--json"]);
    assert!(
        !output.status.success(),
        "health succeeded with no web UI listening"
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be one JSON document");
    assert_eq!(value["daemon_running"], true);
    assert_eq!(value["web_responding"], false);
    assert_eq!(value["web_status"], serde_json::Value::Null);
    assert!(
        value["web_error"]
            .as_str()
            .expect("web_error must be reported")
            .contains("could not connect"),
        "{}",
        value
    );
    assert_eq!(value["healthy"], false);
    let line = assert_one_error_line(&output);
    assert!(line.contains("health check failed"), "{}", line);
}

#[test]
fn health_fails_when_the_daemon_is_inactive_even_if_the_web_answers() {
    let _guard = port_lock();
    let daemon = FakeDaemon::new("daemon-down", "inactive");
    let _web = start_web_server("HTTP/1.0 200 OK");

    let output = daemon.run_health(&[]);
    assert!(
        !output.status.success(),
        "health succeeded with an inactive daemon"
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("daemon: not running"), "{}", stdout);
    assert!(stdout.contains("web: responding (HTTP 200)"), "{}", stdout);
    assert!(stdout.contains("healthy: no"), "{}", stdout);
    let line = assert_one_error_line(&output);
    assert!(line.contains("daemon is not running"), "{}", line);
}

#[test]
fn health_fails_when_the_web_answers_500() {
    let _guard = port_lock();
    let daemon = FakeDaemon::new("web-500", "active");
    let _web = start_web_server("HTTP/1.0 500 Internal Server Error");

    let output = daemon.run_health(&["--json"]);
    assert!(
        !output.status.success(),
        "health succeeded when the web UI answered 500"
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["web_responding"], true);
    assert_eq!(value["web_status"], 500);
    assert_eq!(value["healthy"], false);
    let line = assert_one_error_line(&output);
    assert!(line.contains("HTTP 500"), "{}", line);
}
