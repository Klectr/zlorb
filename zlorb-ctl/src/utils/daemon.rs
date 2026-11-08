use std::process::{Command, ExitStatus};
use zlorb_lib::{error::ZlorbError, is_root, log::Logger};

pub struct DaemonManager {}
impl DaemonManager {
    pub fn start() -> Result<(), ZlorbError> {
        let out_status = DaemonManager::run_process_check()?;
        let is_running = out_status.success();
        match is_running {
            true => DaemonManager::run_service_restart_cmd()?,
            false => DaemonManager::run_service_start_cmd()?,
        };
        Ok(())
    }

    pub fn is_running() -> Result<bool, ZlorbError> {
        Ok(DaemonManager::run_process_check()?.success())
    }

    fn run_process_check() -> Result<ExitStatus, ZlorbError> {
        let mut cmd = Command::new("systemctl");
        if !is_root() {
            cmd.arg("--user");
        }
        cmd.args(["is-active", "--quiet", "zlorb"])
            .status()
            .map_err(ZlorbError::Io)
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

    fn run_service_restart_cmd() -> Result<(), ZlorbError> {
        Logger::info("Daemon already running, restarting it".into());
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
    fn successful_start_and_restart_statuses_pass() {
        assert!(ensure_daemon_action_succeeded_in_scope(status(0), "start", "--user ").is_ok());
        assert!(ensure_daemon_action_succeeded_in_scope(status(0), "restart", "").is_ok());
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
