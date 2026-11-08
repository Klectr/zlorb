use std::process::{Command, ExitStatus};
use zlorb_lib::{error::ZlorbError, is_root, log::Logger};

pub struct DaemonManager {}
impl DaemonManager {
    pub fn start() -> Result<(), ZlorbError> {
        DaemonManager::start_with(
            DaemonManager::run_daemon_state(),
            DaemonManager::run_service_start_cmd,
        )
    }

    fn start_with(
        state: Result<String, ZlorbError>,
        run: impl FnOnce() -> Result<(), ZlorbError>,
    ) -> Result<(), ZlorbError> {
        if start_action(&state?) == DaemonAction::NoOp {
            Logger::info("Daemon already running; nothing to start".into());
            return Ok(());
        }
        run()
    }

    pub fn stop() -> Result<(), ZlorbError> {
        DaemonManager::stop_with(
            DaemonManager::run_daemon_state(),
            DaemonManager::run_service_stop_cmd,
        )
    }

    fn stop_with(
        state: Result<String, ZlorbError>,
        run: impl FnOnce() -> Result<(), ZlorbError>,
    ) -> Result<(), ZlorbError> {
        if stop_action(&state?) == DaemonAction::NoOp {
            Logger::info("Daemon is not running; nothing to stop".into());
            return Ok(());
        }
        run()
    }

    pub fn restart() -> Result<(), ZlorbError> {
        DaemonManager::run_service_restart_cmd()
    }

    pub fn is_running() -> Result<bool, ZlorbError> {
        DaemonManager::is_running_with(DaemonManager::run_daemon_state())
    }

    fn is_running_with(state: Result<String, ZlorbError>) -> Result<bool, ZlorbError> {
        Ok(state? == "active")
    }

    fn run_daemon_state() -> Result<String, ZlorbError> {
        let user_scope = !is_root();
        let mut cmd = Command::new("systemctl");
        if user_scope {
            cmd.arg("--user");
        }
        let output = cmd
            .args(["is-active", "zlorb"])
            .output()
            .map_err(ZlorbError::Io)?;
        parse_daemon_state(
            &String::from_utf8_lossy(&output.stdout),
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
            if user_scope { "--user " } else { "" },
        )
    }

    fn run_service_start_cmd() -> Result<(), ZlorbError> {
        Logger::info("Starting the daemon".into());
        let mut cmd = Command::new("systemctl");
        if !is_root() {
            cmd.arg("--user");
        }
        let status = cmd
            .args(["start", "zlorb"])
            .status()
            .map_err(ZlorbError::Io)?;
        ensure_daemon_action_succeeded(status, "start")?;
        Logger::info("...Started".into());
        Ok(())
    }

    fn run_service_stop_cmd() -> Result<(), ZlorbError> {
        Logger::info("Stopping the daemon".into());
        let mut cmd = Command::new("systemctl");
        if !is_root() {
            cmd.arg("--user");
        }
        let status = cmd
            .args(["stop", "zlorb"])
            .status()
            .map_err(ZlorbError::Io)?;
        ensure_daemon_action_succeeded(status, "stop")?;
        Logger::info("...Stopped".into());
        Ok(())
    }

    fn run_service_restart_cmd() -> Result<(), ZlorbError> {
        Logger::info("Restarting the daemon".into());
        let mut cmd = Command::new("systemctl");
        if !is_root() {
            cmd.arg("--user");
        }
        let status = cmd
            .args(["restart", "zlorb"])
            .status()
            .map_err(ZlorbError::Io)?;
        ensure_daemon_action_succeeded(status, "restart")?;
        Logger::info("...Restarted".into());
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DaemonAction {
    NoOp,
    Run,
}

fn start_action(state: &str) -> DaemonAction {
    if state == "active" {
        DaemonAction::NoOp
    } else {
        DaemonAction::Run
    }
}

fn stop_action(state: &str) -> DaemonAction {
    if state == "inactive" {
        DaemonAction::NoOp
    } else {
        DaemonAction::Run
    }
}

fn parse_daemon_state(
    raw: &str,
    exit_code: Option<i32>,
    stderr: &str,
    scope: &str,
) -> Result<String, ZlorbError> {
    let state = raw.trim();
    if state.is_empty() || !matches!(exit_code, Some(0) | Some(3)) {
        let exit_text = match exit_code {
            Some(code) => format!("exit status {}", code),
            None => "termination by signal".to_string(),
        };
        let detail = stderr.trim();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!("; systemctl said: {}", detail)
        };
        return Err(ZlorbError::Other(format!(
            "could not read the daemon state (systemctl {}is-active zlorb: {}) — inspect it with: systemctl {}status zlorb{}",
            scope, exit_text, scope, suffix
        )));
    }
    Ok(state.to_string())
}

fn systemctl_scope() -> &'static str {
    if is_root() {
        ""
    } else {
        "--user "
    }
}

fn ensure_daemon_action_succeeded(status: ExitStatus, action: &str) -> Result<(), ZlorbError> {
    ensure_daemon_action_succeeded_in_scope(status, action, systemctl_scope())
}

fn ensure_daemon_action_succeeded_in_scope(
    status: ExitStatus,
    action: &str,
    scope: &str,
) -> Result<(), ZlorbError> {
    if status.success() {
        return Ok(());
    }
    Err(ZlorbError::Other(format!(
        "systemctl {}{} zlorb failed ({}); the daemon did not {} — inspect it with: systemctl {}status zlorb",
        scope, action, status, action, scope
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(exit_code: i32) -> ExitStatus {
        Command::new("sh")
            .args(["-c", &format!("exit {}", exit_code)])
            .status()
            .unwrap()
    }

    #[test]
    fn successful_start_stop_and_restart_statuses_pass() {
        assert!(ensure_daemon_action_succeeded_in_scope(status(0), "start", "--user ").is_ok());
        assert!(ensure_daemon_action_succeeded_in_scope(status(0), "stop", "--user ").is_ok());
        assert!(ensure_daemon_action_succeeded_in_scope(status(0), "restart", "").is_ok());
    }

    #[test]
    fn failed_stop_status_is_an_error_naming_the_action_and_the_inspect_command() {
        let err = format!(
            "{:?}",
            ensure_daemon_action_succeeded_in_scope(status(3), "stop", "--user ").unwrap_err()
        );
        assert!(err.contains("systemctl --user stop zlorb failed"), "{}", err);
        assert!(err.contains("exit status: 3"), "{}", err);
        assert!(err.contains("the daemon did not stop"), "{}", err);
        assert!(err.contains("systemctl --user status zlorb"), "{}", err);
    }

    #[test]
    fn failed_start_status_is_an_error_naming_the_action_and_the_inspect_command() {
        let err = format!(
            "{:?}",
            ensure_daemon_action_succeeded_in_scope(status(3), "start", "--user ").unwrap_err()
        );
        assert!(err.contains("systemctl --user start zlorb failed"), "{}", err);
        assert!(err.contains("exit status: 3"), "{}", err);
        assert!(err.contains("the daemon did not start"), "{}", err);
        assert!(err.contains("systemctl --user status zlorb"), "{}", err);
    }

    #[test]
    fn systemctl_scope_is_empty_or_user_with_its_trailing_space() {
        assert!(
            systemctl_scope() == "" || systemctl_scope() == "--user ",
            "{:?}",
            systemctl_scope()
        );
    }

    #[test]
    fn parse_daemon_state_trims_the_state_string() {
        assert_eq!(
            parse_daemon_state("active\n", Some(0), "", "--user ").unwrap(),
            "active"
        );
        assert_eq!(
            parse_daemon_state("  inactive \n", Some(3), "", "").unwrap(),
            "inactive"
        );
        assert_eq!(
            parse_daemon_state("failed\n", Some(3), "", "").unwrap(),
            "failed"
        );
        assert_eq!(
            parse_daemon_state("activating\n", Some(3), "", "").unwrap(),
            "activating"
        );
    }

    #[test]
    fn parse_daemon_state_with_no_state_is_an_error_naming_the_inspect_command() {
        let err = format!(
            "{:?}",
            parse_daemon_state("", Some(3), "", "--user ").unwrap_err()
        );
        assert!(err.contains("could not read the daemon state"), "{}", err);
        assert!(
            err.contains("systemctl --user is-active zlorb"),
            "{}",
            err
        );
        assert!(err.contains("systemctl --user status zlorb"), "{}", err);
        let root_err = format!(
            "{:?}",
            parse_daemon_state("  \n", Some(3), "", "").unwrap_err()
        );
        assert!(root_err.contains("systemctl status zlorb"), "{}", root_err);
        assert!(!root_err.contains("--user"), "{}", root_err);
    }

    #[test]
    fn parse_daemon_state_rejects_a_failed_query_even_with_plausible_stdout() {
        let err = format!(
            "{:?}",
            parse_daemon_state(
                "inactive\n",
                Some(1),
                "Failed to connect to bus: No medium found",
                "--user "
            )
            .unwrap_err()
        );
        assert!(err.contains("could not read the daemon state"), "{}", err);
        assert!(err.contains("exit status 1"), "{}", err);
        assert!(
            err.contains("Failed to connect to bus: No medium found"),
            "{}",
            err
        );
        assert!(err.contains("systemctl --user status zlorb"), "{}", err);
        let unknown_unit = format!(
            "{:?}",
            parse_daemon_state("inactive\n", Some(4), "", "").unwrap_err()
        );
        assert!(unknown_unit.contains("exit status 4"), "{}", unknown_unit);
        let signalled = format!(
            "{:?}",
            parse_daemon_state("active\n", None, "", "").unwrap_err()
        );
        assert!(signalled.contains("termination by signal"), "{}", signalled);
    }

    #[test]
    fn stop_with_no_ops_on_inactive_and_runs_the_verb_on_other_states() {
        use std::cell::Cell;
        let called = Cell::new(false);
        let result = DaemonManager::stop_with(Ok("inactive".to_string()), || {
            called.set(true);
            Ok(())
        });
        assert!(result.is_ok());
        assert!(!called.get());
        for state in ["active", "failed", "activating", "deactivating"] {
            let called = Cell::new(false);
            let result = DaemonManager::stop_with(Ok(state.to_string()), || {
                called.set(true);
                Ok(())
            });
            assert!(result.is_ok(), "{}", state);
            assert!(called.get(), "{}", state);
        }
    }

    #[test]
    fn start_with_no_ops_on_active_and_runs_the_verb_on_other_states() {
        use std::cell::Cell;
        let called = Cell::new(false);
        let result = DaemonManager::start_with(Ok("active".to_string()), || {
            called.set(true);
            Ok(())
        });
        assert!(result.is_ok());
        assert!(!called.get());
        for state in ["inactive", "failed", "activating"] {
            let called = Cell::new(false);
            let result = DaemonManager::start_with(Ok(state.to_string()), || {
                called.set(true);
                Ok(())
            });
            assert!(result.is_ok(), "{}", state);
            assert!(called.get(), "{}", state);
        }
    }

    #[test]
    fn start_and_stop_propagate_a_state_read_error_without_running_the_verb() {
        use std::cell::Cell;
        let called = Cell::new(false);
        let result = DaemonManager::stop_with(
            Err(ZlorbError::Other("no state".to_string())),
            || {
                called.set(true);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!called.get());
        let called = Cell::new(false);
        let result = DaemonManager::start_with(
            Err(ZlorbError::Other("no state".to_string())),
            || {
                called.set(true);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!called.get());
    }

    #[test]
    fn start_and_stop_propagate_a_verb_error() {
        let stop_result = DaemonManager::stop_with(Ok("active".to_string()), || {
            Err(ZlorbError::Other("stop failed".to_string()))
        });
        assert!(format!("{:?}", stop_result.unwrap_err()).contains("stop failed"));
        let start_result = DaemonManager::start_with(Ok("inactive".to_string()), || {
            Err(ZlorbError::Other("start failed".to_string()))
        });
        assert!(format!("{:?}", start_result.unwrap_err()).contains("start failed"));
    }

    #[test]
    fn is_running_with_is_true_only_for_active_and_propagates_read_errors() {
        assert!(DaemonManager::is_running_with(Ok("active".to_string())).unwrap());
        assert!(!DaemonManager::is_running_with(Ok("inactive".to_string())).unwrap());
        assert!(!DaemonManager::is_running_with(Ok("failed".to_string())).unwrap());
        assert!(
            DaemonManager::is_running_with(Err(ZlorbError::Other("no state".to_string())))
                .is_err()
        );
    }

    #[test]
    fn stop_no_ops_only_on_a_positively_read_inactive_state() {
        assert_eq!(stop_action("inactive"), DaemonAction::NoOp);
        assert_eq!(stop_action("active"), DaemonAction::Run);
        assert_eq!(stop_action("failed"), DaemonAction::Run);
        assert_eq!(stop_action("activating"), DaemonAction::Run);
        assert_eq!(stop_action("deactivating"), DaemonAction::Run);
        assert_eq!(stop_action("unknown"), DaemonAction::Run);
    }

    #[test]
    fn start_no_ops_only_on_a_positively_read_active_state() {
        assert_eq!(start_action("active"), DaemonAction::NoOp);
        assert_eq!(start_action("inactive"), DaemonAction::Run);
        assert_eq!(start_action("failed"), DaemonAction::Run);
        assert_eq!(start_action("activating"), DaemonAction::Run);
        assert_eq!(start_action("deactivating"), DaemonAction::Run);
        assert_eq!(start_action("unknown"), DaemonAction::Run);
    }

    #[test]
    fn failed_restart_status_is_an_error_in_the_root_scope() {
        let err = format!(
            "{:?}",
            ensure_daemon_action_succeeded_in_scope(status(1), "restart", "").unwrap_err()
        );
        assert!(err.contains("systemctl restart zlorb failed"), "{}", err);
        assert!(err.contains("the daemon did not restart"), "{}", err);
        assert!(err.contains("systemctl status zlorb"), "{}", err);
        assert!(!err.contains("--user"), "{}", err);
    }
}
