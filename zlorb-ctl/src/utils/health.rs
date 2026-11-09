use serde::Serialize;
use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};
use zlorb_lib::error::ZlorbError;

use crate::utils::daemon::DaemonManager;

pub const WEB_PROBE_ADDR: &str = "127.0.0.1:3000";
const WEB_HEALTH_PATH: &str = "/healthz";
const WEB_IDENTITY_BODY: &str = "zlorb-web ok";
const PROBE_DEADLINE: Duration = Duration::from_secs(3);
const MAX_RESPONSE_BYTES: usize = 8192;

#[derive(Debug, Serialize, PartialEq)]
pub struct HealthReport {
    pub daemon_running: bool,
    pub daemon_error: Option<String>,
    pub web_responding: bool,
    pub web_status: Option<u16>,
    pub web_error: Option<String>,
    pub healthy: bool,
}

#[derive(Debug, PartialEq)]
pub struct WebProbe {
    pub status: Option<u16>,
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

fn parse_http_status(status_line: &str) -> Option<u16> {
    let mut parts = status_line.split_whitespace();
    let version = parts.next()?;
    let version_tail = version.strip_prefix("HTTP/")?;
    if version_tail.is_empty()
        || !version_tail
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.')
    {
        return None;
    }
    let code: u16 = parts.next()?.parse().ok()?;
    if !(100..=599).contains(&code) {
        return None;
    }
    Some(code)
}

fn parse_web_response(addr: &str, bytes: &[u8]) -> WebProbe {
    let text = String::from_utf8_lossy(bytes);
    let Some(line_end) = text.find('\n') else {
        return WebProbe {
            status: None,
            error: Some(format!(
                "the web UI at {} did not send a complete HTTP status line",
                addr
            )),
        };
    };
    let status_line = text[..line_end].trim_end_matches('\r');
    let Some(status) = parse_http_status(status_line) else {
        return WebProbe {
            status: None,
            error: Some(format!(
                "the web UI at {} sent a response that is not HTTP",
                addr
            )),
        };
    };
    if (200..300).contains(&status) {
        let body = match text.find("\r\n\r\n") {
            Some(index) => &text[index + 4..],
            None => match text.find("\n\n") {
                Some(index) => &text[index + 2..],
                None => "",
            },
        };
        if body.trim() != WEB_IDENTITY_BODY {
            return WebProbe {
                status: None,
                error: Some(format!(
                    "a server at {} answered HTTP {} but did not identify as the zlorb web UI",
                    addr, status
                )),
            };
        }
    }
    WebProbe {
        status: Some(status),
        error: None,
    }
}

pub fn probe_web_at(addr: &str) -> WebProbe {
    probe_web_within(addr, PROBE_DEADLINE)
}

fn probe_web_within(addr: &str, deadline: Duration) -> WebProbe {
    let started = Instant::now();
    let socket_addr = match addr.to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(socket_addr) => socket_addr,
            None => {
                return WebProbe {
                    status: None,
                    error: Some(format!("could not resolve the web address {}", addr)),
                };
            }
        },
        Err(e) => {
            return WebProbe {
                status: None,
                error: Some(format!(
                    "could not resolve the web address {}: {}",
                    addr, e
                )),
            };
        }
    };
    let remaining = deadline
        .checked_sub(started.elapsed())
        .unwrap_or(Duration::from_millis(1));
    let mut stream = match TcpStream::connect_timeout(&socket_addr, remaining) {
        Ok(stream) => stream,
        Err(e) => {
            return WebProbe {
                status: None,
                error: Some(format!(
                    "could not connect to the web UI at {}: {}",
                    addr, e
                )),
            };
        }
    };
    let remaining = deadline
        .checked_sub(started.elapsed())
        .unwrap_or(Duration::from_millis(1));
    let _ = stream.set_write_timeout(Some(remaining));
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n",
        WEB_HEALTH_PATH
    );
    if let Err(e) = stream.write_all(request.as_bytes()) {
        return WebProbe {
            status: None,
            error: Some(format!(
                "could not request the web UI at {}: {}",
                addr, e
            )),
        };
    }
    let mut bytes: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if bytes.len() >= MAX_RESPONSE_BYTES {
            break;
        }
        let Some(remaining) = deadline.checked_sub(started.elapsed()) else {
            if bytes.is_empty() {
                return WebProbe {
                    status: None,
                    error: Some(format!(
                        "the web UI at {} did not respond within {} seconds",
                        addr,
                        deadline.as_secs()
                    )),
                };
            }
            break;
        };
        let _ = stream.set_read_timeout(Some(remaining));
        let room = MAX_RESPONSE_BYTES - bytes.len();
        let take = room.min(chunk.len());
        match stream.read(&mut chunk[..take]) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if bytes.is_empty() {
                    return WebProbe {
                        status: None,
                        error: Some(format!(
                            "the web UI at {} did not respond within {} seconds",
                            addr,
                            deadline.as_secs()
                        )),
                    };
                }
                break;
            }
            Err(e) => {
                if bytes.is_empty() {
                    return WebProbe {
                        status: None,
                        error: Some(format!(
                            "could not read the web UI response at {}: {}",
                            addr, e
                        )),
                    };
                }
                break;
            }
        }
    }
    if bytes.is_empty() {
        return WebProbe {
            status: None,
            error: Some(format!(
                "the web UI at {} closed the connection without responding",
                addr
            )),
        };
    }
    parse_web_response(addr, &bytes)
}

pub fn build_report(daemon: Result<bool, ZlorbError>, web: WebProbe) -> HealthReport {
    let (daemon_running, daemon_error) = match daemon {
        Ok(running) => (running, None),
        Err(e) => (false, Some(error_text(&e))),
    };
    let web_responding = web.status.is_some();
    let healthy =
        daemon_running && matches!(web.status, Some(status) if (200..300).contains(&status));
    HealthReport {
        daemon_running,
        daemon_error,
        web_responding,
        web_status: web.status,
        web_error: web.error,
        healthy,
    }
}

fn failure_reasons(report: &HealthReport) -> Vec<String> {
    let mut reasons = Vec::new();
    if let Some(err) = &report.daemon_error {
        reasons.push(format!("daemon check failed: {}", err));
    } else if !report.daemon_running {
        reasons.push("daemon is not running".to_string());
    }
    if let Some(err) = &report.web_error {
        reasons.push(err.clone());
    } else if let Some(status) = report.web_status {
        if !(200..300).contains(&status) {
            reasons.push(format!("web UI responded with HTTP {}", status));
        }
    } else {
        reasons.push("web UI is not responding".to_string());
    }
    reasons
}

pub(crate) fn health(json: bool) -> Result<(), ZlorbError> {
    let report = build_report(DaemonManager::is_running(), probe_web_at(WEB_PROBE_ADDR));
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(rendered) => println!("{}", rendered),
            Err(e) => {
                eprintln!("error: could not serialize the health report: {}", e);
                std::process::exit(1);
            }
        }
    } else {
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
        match report.web_status {
            Some(status) => println!("web: responding (HTTP {})", status),
            None => println!("web: not responding"),
        }
        if let Some(err) = &report.web_error {
            println!("web check failed: {}", err);
        }
        println!("healthy: {}", if report.healthy { "yes" } else { "no" });
    }
    if report.healthy {
        Ok(())
    } else {
        Err(ZlorbError::Other(format!(
            "health check failed: {}",
            failure_reasons(&report).join("; ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn serve_once(response: &'static str, foreign: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf);
                let request = String::from_utf8_lossy(&buf);
                if !foreign && !request.starts_with("GET /healthz ") {
                    let _ = stream.write_all(
                        b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                    );
                    return;
                }
                let _ = stream.write_all(response.as_bytes());
            }
        });
        addr
    }

    fn closed_addr() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        addr
    }

    const OK_RESPONSE: &str =
        "HTTP/1.0 200 OK\r\nContent-Length: 13\r\n\r\nzlorb-web ok\n";

    #[test]
    fn parse_http_status_reads_the_code_from_a_status_line() {
        assert_eq!(parse_http_status("HTTP/1.1 200 OK"), Some(200));
        assert_eq!(
            parse_http_status("HTTP/1.0 500 Internal Server Error"),
            Some(500)
        );
        assert_eq!(parse_http_status("not http at all"), None);
        assert_eq!(parse_http_status("HTTP/1.1 nope"), None);
        assert_eq!(parse_http_status("HTTP/anything 200 OK"), None);
        assert_eq!(parse_http_status("HTTP/1.1 99 Weird"), None);
        assert_eq!(parse_http_status("HTTP/1.1 600 Weird"), None);
        assert_eq!(parse_http_status(""), None);
    }

    #[test]
    fn probe_reads_the_status_from_a_live_listener() {
        let addr = serve_once(OK_RESPONSE, false);
        let probe = probe_web_at(&addr);
        assert_eq!(probe.status, Some(200));
        assert_eq!(probe.error, None);
    }

    #[test]
    fn probe_rejects_a_foreign_server_that_answers_200() {
        let addr = serve_once(
            "HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nhello",
            true,
        );
        let probe = probe_web_at(&addr);
        assert_eq!(probe.status, None);
        let err = probe.error.expect("foreign server must error");
        assert!(err.contains("did not identify"), "{}", err);
    }

    #[test]
    fn probe_rejects_a_body_that_only_mentions_zlorb_web() {
        let addr = serve_once(
            "HTTP/1.0 200 OK\r\nContent-Length: 24\r\n\r\nwelcome zlorb-web fans",
            true,
        );
        let probe = probe_web_at(&addr);
        assert_eq!(probe.status, None);
        let err = probe.error.expect("mention-only body must error");
        assert!(err.contains("did not identify"), "{}", err);
    }

    #[test]
    fn probe_of_a_closed_port_reports_a_connect_error() {
        let mut connect_error = None;
        for _ in 0..8 {
            let probe = probe_web_at(&closed_addr());
            if probe.status.is_none()
                && probe
                    .error
                    .as_deref()
                    .is_some_and(|err| err.contains("could not connect"))
            {
                connect_error = probe.error;
                break;
            }
        }
        let err = connect_error.expect(
            "no closed port produced a connect error; a freed ephemeral port may have been reused by another test listener on every attempt",
        );
        assert!(err.contains("could not connect"), "{}", err);
    }

    #[test]
    fn probe_of_a_non_http_responder_reports_it_without_echoing_it() {
        let addr = serve_once("hello \x1b[31mthere\r\n", true);
        let probe = probe_web_at(&addr);
        assert_eq!(probe.status, None);
        let err = probe.error.expect("non-HTTP response must error");
        assert!(err.contains("not HTTP"), "{}", err);
        assert!(!err.contains('\x1b'), "{}", err);
        assert!(!err.contains("hello"), "{}", err);
    }

    #[test]
    fn probe_rejects_a_status_line_that_is_never_terminated() {
        let addr = serve_once("HTTP/1.1 200 OK", true);
        let probe = probe_web_at(&addr);
        assert_eq!(probe.status, None);
        let err = probe.error.expect("unterminated status line must error");
        assert!(err.contains("complete HTTP status line"), "{}", err);
    }

    #[test]
    fn probe_bounds_a_stream_of_garbage_without_a_status_line() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf);
                let garbage = vec![b'a'; 65536];
                let _ = stream.write_all(&garbage);
            }
        });
        let probe = probe_web_within(&addr, Duration::from_secs(3));
        assert_eq!(probe.status, None);
        assert!(probe.error.is_some());
    }

    #[test]
    fn probe_times_out_on_a_trickling_responder() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf);
                for _ in 0..100 {
                    if stream.write_all(b"H").is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });
        let started = Instant::now();
        let probe = probe_web_within(&addr, Duration::from_millis(400));
        assert_eq!(probe.status, None);
        assert!(probe.error.is_some());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "probe ignored its deadline: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn report_is_healthy_only_when_the_daemon_runs_and_the_web_answers_2xx() {
        let ok = build_report(
            Ok(true),
            WebProbe {
                status: Some(200),
                error: None,
            },
        );
        assert!(ok.healthy);
        assert!(ok.web_responding);

        let web_down = build_report(
            Ok(true),
            WebProbe {
                status: None,
                error: Some("could not connect".to_string()),
            },
        );
        assert!(!web_down.healthy);
        assert!(!web_down.web_responding);
        assert_eq!(web_down.web_error.as_deref(), Some("could not connect"));

        let daemon_down = build_report(
            Ok(false),
            WebProbe {
                status: Some(200),
                error: None,
            },
        );
        assert!(!daemon_down.healthy);
        assert!(daemon_down.web_responding);
    }

    #[test]
    fn report_treats_a_500_as_responding_but_not_healthy() {
        let report = build_report(
            Ok(true),
            WebProbe {
                status: Some(500),
                error: None,
            },
        );
        assert!(report.web_responding);
        assert_eq!(report.web_status, Some(500));
        assert!(!report.healthy);
        let reasons = failure_reasons(&report);
        assert_eq!(
            reasons,
            vec!["web UI responded with HTTP 500".to_string()]
        );
    }

    #[test]
    fn report_carries_a_daemon_read_error_and_is_not_healthy() {
        let report = build_report(
            Err(ZlorbError::Other("no systemctl".to_string())),
            WebProbe {
                status: Some(200),
                error: None,
            },
        );
        assert!(!report.daemon_running);
        assert_eq!(report.daemon_error.as_deref(), Some("no systemctl"));
        assert!(!report.healthy);
        let reasons = failure_reasons(&report);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("no systemctl"), "{}", reasons[0]);
    }

    #[test]
    fn report_serializes_with_the_documented_field_names() {
        let report = build_report(
            Ok(true),
            WebProbe {
                status: Some(200),
                error: None,
            },
        );
        let value: serde_json::Value = serde_json::to_value(&report).unwrap();
        let object = value.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(|key| key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "daemon_error",
                "daemon_running",
                "healthy",
                "web_error",
                "web_responding",
                "web_status",
            ]
        );
        assert_eq!(value["healthy"], serde_json::json!(true));
        assert_eq!(value["web_status"], serde_json::json!(200));
    }
}
