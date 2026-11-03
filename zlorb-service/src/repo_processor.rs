use git2::{
    AnnotatedCommit, Branch, BranchType, Cred, FetchOptions, MergeAnalysis, MergePreference, Oid,
    Reference, Remote, RemoteCallbacks,
};
use std::{
    collections::HashMap,
    fmt::Debug,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};
use zlorb_lib::{config::RepositoryConfiguration, error::ZlorbError, log::Logger};

use crate::build_system_executor::BuildSystemExecutor;

pub type FailedBuilds = Arc<Mutex<HashMap<String, Oid>>>;

pub fn new_failed_builds() -> FailedBuilds {
    Arc::new(Mutex::new(HashMap::new()))
}

pub type FailedRestarts = Arc<Mutex<HashMap<String, Oid>>>;

pub fn new_failed_restarts() -> FailedRestarts {
    Arc::new(Mutex::new(HashMap::new()))
}

fn kill_process_group(pgid: libc::pid_t) {
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

fn pipe_drain_deadline() -> std::time::Instant {
    std::time::Instant::now() + std::time::Duration::from_secs(10)
}

fn pipe_target(fd: libc::c_int) -> Option<String> {
    let target = std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()?;
    let target = target.to_string_lossy().into_owned();
    target.starts_with("pipe:[").then_some(target)
}

fn process_start_time(pid: libc::pid_t) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    stat[close + 1..].split_whitespace().nth(19)?.parse().ok()
}

fn process_pipe_write_targets(pid: libc::pid_t, targets: &[String]) -> Vec<String> {
    let mut held = Vec::new();
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return held;
    };
    for entry in entries.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let target = target.to_string_lossy().into_owned();
        if !targets.iter().any(|candidate| candidate == &target) || held.contains(&target) {
            continue;
        }
        let Ok(info) =
            std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{}", entry.file_name().to_string_lossy()))
        else {
            continue;
        };
        let writable = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .and_then(|flags| i64::from_str_radix(flags.trim(), 8).ok())
            .map(|flags| flags & 0o3 != 0)
            .unwrap_or(false);
        if writable {
            held.push(target);
        }
    }
    held
}

fn kill_pipe_holders(targets: &[String]) {
    if targets.is_empty() {
        return;
    }
    let own = std::process::id() as libc::pid_t;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<libc::pid_t>() else {
            continue;
        };
        if pid == own {
            continue;
        }
        if process_pipe_write_targets(pid, targets).is_empty() {
            continue;
        }
        let Some(started) = process_start_time(pid) else {
            continue;
        };
        let held = process_pipe_write_targets(pid, targets);
        if process_start_time(pid) != Some(started) || held.is_empty() {
            continue;
        }
        if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
            Logger::warn(format!(
                "Killed process {} holding the output pipe {} open after the drain deadline",
                pid,
                held.join(", ")
            ));
        } else {
            Logger::error(format!(
                "Could not kill process {} holding the output pipe {} open after the drain deadline: {}",
                pid,
                held.join(", "),
                std::io::Error::last_os_error()
            ));
        }
    }
}

fn set_nonblocking(fd: libc::c_int) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

struct PipeDrain {
    reader: Box<dyn std::io::Read>,
    fd: libc::c_int,
    buf: Vec<u8>,
    eof: bool,
}

impl PipeDrain {
    fn new<R: std::io::Read + std::os::unix::io::AsRawFd + 'static>(reader: R) -> Self {
        let fd = reader.as_raw_fd();
        set_nonblocking(fd);
        Self {
            reader: Box::new(reader),
            fd,
            buf: Vec::new(),
            eof: false,
        }
    }

    fn read_available(&mut self) {
        let mut chunk = [0u8; 8192];
        for _ in 0..128 {
            match self.reader.read(&mut chunk) {
                Ok(0) => {
                    self.eof = true;
                    return;
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.eof = true;
                    return;
                }
            }
        }
    }
}

struct PipeDrains {
    stdout: PipeDrain,
    stderr: PipeDrain,
}

impl PipeDrains {
    fn new<O, E>(stdout: O, stderr: E) -> Self
    where
        O: std::io::Read + std::os::unix::io::AsRawFd + 'static,
        E: std::io::Read + std::os::unix::io::AsRawFd + 'static,
    {
        Self {
            stdout: PipeDrain::new(stdout),
            stderr: PipeDrain::new(stderr),
        }
    }

    fn all_eof(&self) -> bool {
        self.stdout.eof && self.stderr.eof
    }

    fn poll_ready(&mut self, wait: std::time::Duration) {
        if self.all_eof() {
            std::thread::sleep(wait);
            return;
        }
        let mut fds = Vec::new();
        let mut is_stdout = Vec::new();
        for (drain, flag) in [(&self.stdout, true), (&self.stderr, false)] {
            if !drain.eof {
                fds.push(libc::pollfd {
                    fd: drain.fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
                is_stdout.push(flag);
            }
        }
        let timeout = wait.as_millis().min(1000) as libc::c_int;
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if ready <= 0 {
            return;
        }
        for (index, fd) in fds.iter().enumerate() {
            if fd.revents == 0 {
                continue;
            }
            let drain = if is_stdout[index] {
                &mut self.stdout
            } else {
                &mut self.stderr
            };
            if fd.revents & libc::POLLNVAL != 0 {
                drain.eof = true;
            } else if fd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                drain.read_available();
            }
        }
    }

    fn drain_until(&mut self, end: std::time::Instant) {
        while !self.all_eof() {
            let remaining = end.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            self.poll_ready(remaining.min(std::time::Duration::from_millis(100)));
        }
    }

    fn finish(&mut self, deadline: std::time::Instant, pgid: libc::pid_t) -> bool {
        let first_end = deadline.min(std::time::Instant::now() + std::time::Duration::from_secs(5));
        self.drain_until(first_end);
        let mut sweeps = 0;
        while !self.all_eof() {
            let targets: Vec<String> = [&self.stdout, &self.stderr]
                .iter()
                .filter(|drain| !drain.eof)
                .filter_map(|drain| pipe_target(drain.fd))
                .collect();
            kill_process_group(pgid);
            kill_pipe_holders(&targets);
            sweeps += 1;
            let grace_end = std::time::Instant::now() + std::time::Duration::from_millis(250);
            if sweeps >= 2 {
                self.drain_until(deadline.max(grace_end));
                break;
            }
            self.drain_until(grace_end);
        }
        self.stdout.eof
    }
}

pub fn tracker_key(raw_path: &str) -> String {
    match std::fs::canonicalize(raw_path) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(_) => normalize_path_lexically(raw_path),
    }
}

fn stored_tracker_key(raw_path: &str, repo_name: &str) -> String {
    match std::fs::canonicalize(raw_path) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(e) => {
            Logger::error(format!(
                "Could not canonicalize the path for {} ({}): {}; failed-build and failed-restart tracking will use the normalized path spelling, which may not match other spellings of the same repo",
                repo_name, raw_path, e
            ));
            normalize_path_lexically(raw_path)
        }
    }
}

fn normalize_path_lexically(raw_path: &str) -> String {
    let path = Path::new(raw_path);
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                match normalized.components().last() {
                    Some(std::path::Component::Normal(_)) => {
                        normalized.pop();
                    }
                    Some(std::path::Component::ParentDir) => normalized.push(component),
                    None => {
                        if !path.is_absolute() {
                            normalized.push(component);
                        }
                    }
                    _ => {}
                }
            }
            other => normalized.push(other),
        }
    }
    if normalized.as_os_str().is_empty() {
        return ".".to_string();
    }
    normalized.to_string_lossy().into_owned()
}

fn tracker_failed_at(
    tracker: &FailedBuilds,
    key: &str,
    oid: Oid,
    label: &str,
    repo_name: &str,
) -> bool {
    match tracker.lock() {
        Ok(failed) => failed.get(key) == Some(&oid),
        Err(e) => {
            Logger::error(format!(
                "The {} tracker lock is poisoned for {}; treating the commit as not failed and skipping retries until the daemon restarts: {}",
                label, repo_name, e
            ));
            false
        }
    }
}

fn tracker_record(tracker: &FailedBuilds, key: &str, oid: Oid, label: &str, repo_name: &str) {
    match tracker.lock() {
        Ok(mut failed) => {
            failed.insert(key.to_string(), oid);
        }
        Err(e) => Logger::error(format!(
            "The {} tracker lock is poisoned for {}; the failure at {} was not recorded and will not be retried: {}",
            label, repo_name, oid, e
        )),
    }
}

fn tracker_clear(tracker: &FailedBuilds, key: &str, label: &str, repo_name: &str) {
    match tracker.lock() {
        Ok(mut failed) => {
            failed.remove(key);
        }
        Err(e) => Logger::error(format!(
            "The {} tracker lock is poisoned for {}; the stored failure was not cleared: {}",
            label, repo_name, e
        )),
    }
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
}

fn stat_matches_index_entry(entry: &git2::IndexEntry, metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    entry.ino != 0
        && metadata.ino() as u32 == entry.ino
        && metadata.len() == u64::from(entry.file_size)
        && metadata.mtime() == i64::from(entry.mtime.seconds())
        && metadata.mtime_nsec() == i64::from(entry.mtime.nanoseconds())
        && metadata.ctime() == i64::from(entry.ctime.seconds())
        && metadata.ctime_nsec() == i64::from(entry.ctime.nanoseconds())
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

fn directory_is_tracked_typechange(
    head_paths: &std::collections::HashSet<Vec<u8>>,
    workdir: &Path,
    prefix: &Path,
) -> bool {
    let mut dir_prefix = {
        use std::os::unix::ffi::OsStrExt;
        prefix.as_os_str().as_bytes().to_vec()
    };
    dir_prefix.push(b'/');
    if !head_paths.iter().any(|path| path.starts_with(&dir_prefix)) {
        return false;
    }
    let mut stack = vec![workdir.join(prefix)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let Ok(file_type) = entry.file_type() else {
                return false;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
                continue;
            }
            let entry_path = entry.path();
            let Ok(relative) = entry_path.strip_prefix(workdir) else {
                return false;
            };
            let relative_bytes = {
                use std::os::unix::ffi::OsStrExt;
                relative.as_os_str().as_bytes()
            };
            if !head_paths.contains(relative_bytes) {
                return false;
            }
        }
    }
    true
}

pub struct RepoProcessor {
    pub(crate) repo_path: PathBuf,
    pub repo: git2::Repository,
    pub config: RepositoryConfiguration,
    pub failed_builds: FailedBuilds,
    pub failed_restarts: FailedRestarts,
    tracker_key: String,
}

impl Debug for RepoProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoProcessor")
            .field("repo_path", &self.repo_path)
            .field("repo", &"Repository")
            .field("config", &self.config)
            .finish()
    }
}

fn run_restart_command(
    command: &str,
    repo_path: &Path,
    repo_name: &str,
) -> Result<(), ZlorbError> {
    Logger::info(format!(
        "Running restart command for {}: {}",
        repo_name, command
    ));
    let capture_path = std::env::temp_dir().join(format!(
        "zlorb-restart-output-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    let capture = std::fs::File::create(&capture_path).map_err(|e| {
        let err = format!(
            "Failed to run restart command for {}: could not create the output capture file: {}",
            repo_name, e
        );
        Logger::error(err.clone());
        ZlorbError::Other(err)
    })?;
    let capture_err = capture.try_clone().map_err(|e| {
        let err = format!(
            "Failed to run restart command for {}: could not clone the output capture file: {}",
            repo_name, e
        );
        Logger::error(err.clone());
        ZlorbError::Other(err)
    })?;
    let mut shell = shell_command(command);
    let mut child = shell
        .current_dir(repo_path)
        .stdout(Stdio::from(capture))
        .stderr(Stdio::from(capture_err))
        .spawn()
        .map_err(|e| {
            let err = format!(
                "Failed to run restart command for {}: {}",
                repo_name, e
            );
            Logger::error(err.clone());
            ZlorbError::Other(err)
        })?;
    let status = child.wait().map_err(|e| {
        let err = format!(
            "Failed to run restart command for {}: {}",
            repo_name, e
        );
        Logger::error(err.clone());
        ZlorbError::Other(err)
    })?;
    let output = std::fs::read_to_string(&capture_path).unwrap_or_default();
    let _ = std::fs::remove_file(&capture_path);
    if !output.trim().is_empty() {
        Logger::info(format!(
            "Restart command for {} output: {}",
            repo_name,
            output.trim()
        ));
    }
    if !status.success() {
        let err = format!(
            "Restart command for {} exited with status {}: {}",
            repo_name,
            status,
            output.trim()
        );
        Logger::error(err.clone());
        return Err(ZlorbError::Other(err));
    }
    Ok(())
}

fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("/bin/sh");
    shell.args(["-c", command]);
    shell
}

impl RepoProcessor {
    pub fn new(config: RepositoryConfiguration) -> Result<Self, ZlorbError> {
        Logger::info(format!("Initializing RepoProcessor for {}", config.name));
        let repo = git2::Repository::open(&config.path).map_err(|e| {
            ZlorbError::Other(format!(
                "Failed to acquire repo {} at {}: {}",
                config.name, config.path, e
            ))
        })?;
        let tracker_key = stored_tracker_key(&config.path, &config.name);
        Ok(Self {
            repo_path: PathBuf::from(&config.path),
            repo,
            config,
            failed_builds: new_failed_builds(),
            failed_restarts: new_failed_restarts(),
            tracker_key,
        })
    }

    pub fn update_from_remote(&self) -> Result<(), ZlorbError> {
        Logger::info(format!("Updating from remote for {}", self.config.name));
        self.config.resolved_build_strategy()?;
        let head_before = self.head_oid("before fetch");
        let has_updates = self.fetch_remote_updates()?;
        Logger::info(format!("Remote updates found: {}", has_updates));

        let out_dir_missing = self.config.out_dir.is_some() && !self.check_if_out_dir_exists()?;
        Logger::info(format!("Out directory missing (expected): {}", out_dir_missing));

        let head_oid = self.head_oid("after fetch");
        let retrying_failed_build = head_oid
            .map(|oid| self.build_failed_at(oid))
            .unwrap_or(false);
        if retrying_failed_build {
            Logger::error(format!(
                "Retrying the failed build for {} at {}: the previous build for this commit did not succeed",
                self.config.name,
                head_oid.map(|oid| oid.to_string()).unwrap_or_default()
            ));
        }

        let should_run_build = has_updates || out_dir_missing || retrying_failed_build;
        Logger::info(format!("Should run build: {}", should_run_build));

        if should_run_build && !has_updates {
            let tracked_changes = self._tracked_local_changes()?;
            if !tracked_changes.is_empty() {
                return Err(ZlorbError::Other(format!(
                    "Refusing to build {}: local changes to tracked files ({}) are not part of the commit being built. Stash the work, move it to another branch, or discard it — do not commit it on this branch, the daemon can only fast-forward this branch",
                    self.config.name,
                    display_paths(&tracked_changes)
                )));
            }
        }

        let build_outcome = match should_run_build {
            true => {
                Logger::info(format!(
                    "Found updates or missing output dir for {}. Pulling changes...",
                    self.config.name
                ));
                let exec = BuildSystemExecutor { processor: self };
                match exec.run_build() {
                    Ok(()) => {
                        self.clear_failed_build();
                        Ok(())
                    }
                    Err(e) => {
                        match self.head_oid("after a failed build") {
                            Some(oid) if Some(oid) == head_oid => {
                                self.record_failed_build(oid);
                            }
                            Some(oid) => Logger::error(format!(
                                "Not recording the failed build for {}: HEAD moved to {} during the build, so the failure cannot be attributed to the commit that was current before it",
                                self.config.name, oid
                            )),
                            None => Logger::error(format!(
                                "Not recording the failed build for {}: HEAD could not be read after the build failed, so the failure will not be retried",
                                self.config.name
                            )),
                        }
                        Err(e)
                    }
                }
            }
            false => {
                Logger::info(format!("No build needed for {}", self.config.name));
                Ok(())
            }
        };
        build_outcome.and_then(|_| {
            self.run_restart_if_needed(head_before, retrying_failed_build, should_run_build)
        })
    }

    fn run_restart_if_needed(
        &self,
        head_before: Option<Oid>,
        retrying_failed_build: bool,
        build_ran_this_cycle: bool,
    ) -> Result<(), ZlorbError> {
        let Some(command) = self.config.resolved_restart_command() else {
            return Ok(());
        };
        let current_oid = self.head_oid("before the restart check");
        let head_changed = current_oid.is_some() && current_oid != head_before;
        let retrying_failed_restart = current_oid
            .map(|oid| self.restart_failed_at(oid))
            .unwrap_or(false);
        if retrying_failed_restart && !head_changed {
            Logger::error(format!(
                "Retrying the failed restart command for {} at {}: the previous restart for this commit did not succeed",
                self.config.name,
                current_oid.map(|oid| oid.to_string()).unwrap_or_default()
            ));
        }
        let needed = head_changed || retrying_failed_restart || retrying_failed_build;
        if !needed {
            return Ok(());
        }
        if !build_ran_this_cycle {
            let tracked_changes = self._tracked_local_changes()?;
            if !tracked_changes.is_empty() {
                return Err(ZlorbError::Other(format!(
                    "Refusing to run the restart command for {}: local changes to tracked files ({}) are not part of the commit being restarted on. Stash the work, move it to another branch, or discard it — do not commit it on this branch, the daemon can only fast-forward this branch",
                    self.config.name,
                    display_paths(&tracked_changes)
                )));
            }
        }
        match run_restart_command(command, &self.repo_path, &self.config.name) {
            Ok(()) => {
                self.clear_failed_restart();
                Ok(())
            }
            Err(e) => {
                match current_oid {
                    Some(oid) => self.record_failed_restart(oid),
                    None => Logger::error(format!(
                        "Not recording the failed restart for {}: HEAD could not be read after the restart failed, so the failure will not be retried",
                        self.config.name
                    )),
                }
                Err(e)
            }
        }
    }

    fn head_oid(&self, context: &str) -> Option<Oid> {
        match self.repo.head() {
            Ok(head) => match head.target() {
                Some(oid) => Some(oid),
                None => {
                    Logger::error(format!(
                        "Could not read HEAD for {} ({}): HEAD has no target, so failed-build and failed-restart tracking is skipped for this read",
                        self.config.name, context
                    ));
                    None
                }
            },
            Err(e) => {
                Logger::error(format!(
                    "Could not read HEAD for {} ({}): {}, so failed-build and failed-restart tracking is skipped for this read",
                    self.config.name, context, e
                ));
                None
            }
        }
    }

    fn restart_failed_at(&self, oid: Oid) -> bool {
        tracker_failed_at(
            &self.failed_restarts,
            &self.tracker_key,
            oid,
            "failed-restart",
            &self.config.name,
        )
    }

    fn record_failed_restart(&self, oid: Oid) {
        tracker_record(
            &self.failed_restarts,
            &self.tracker_key,
            oid,
            "failed-restart",
            &self.config.name,
        );
    }

    fn clear_failed_restart(&self) {
        tracker_clear(
            &self.failed_restarts,
            &self.tracker_key,
            "failed-restart",
            &self.config.name,
        );
    }

    fn build_failed_at(&self, oid: Oid) -> bool {
        tracker_failed_at(
            &self.failed_builds,
            &self.tracker_key,
            oid,
            "failed-build",
            &self.config.name,
        )
    }

    fn record_failed_build(&self, oid: Oid) {
        tracker_record(
            &self.failed_builds,
            &self.tracker_key,
            oid,
            "failed-build",
            &self.config.name,
        );
    }

    fn clear_failed_build(&self) {
        tracker_clear(
            &self.failed_builds,
            &self.tracker_key,
            "failed-build",
            &self.config.name,
        );
    }

    fn check_if_out_dir_exists(&self) -> Result<bool, ZlorbError> {
        Logger::info(format!("Checking out dir existence for {}", self.config.name));
        match &self.config.out_dir {
            Some(path_str) => {
                let path = PathBuf::from(path_str);
                let full_path = if path.is_absolute() {
                    path
                } else {
                    self.repo_path.join(path)
                };
                let exists = full_path.exists();
                Logger::info(format!("Out dir {} exists: {}", full_path.display(), exists));
                Ok(exists)
            }
            None => {
                Logger::info("No out_dir specified in config.".to_string());
                Ok(true)
            }
        }
    }

    fn fetch_remote_updates(&self) -> Result<bool, ZlorbError> {
        let local_branch = self._get_local_branch()?;
        let local_oid = self._get_local_oid_from_branch(local_branch)?;
        let mut remote = self._get_remote()?;
        self._download_new_data(&mut remote);
        let analysis = self._get_analysis()?;
        let (merge_analysis, _, fetch_commit) = analysis;
        if merge_analysis.is_up_to_date() {
            return Ok(false);
        }
        if !merge_analysis.is_fast_forward() {
            return Err(ZlorbError::Git(git2::Error::new(
                git2::ErrorCode::NotFastForward,
                git2::ErrorClass::Invalid,
                "Fast-forward only!",
            )));
        }
        let changed_paths = match self._changed_paths(local_oid, fetch_commit.id()) {
            Ok(paths) => Some(paths),
            Err(e) => {
                Logger::error(format!(
                    "Could not compute the paths changed by the update for {}; falling back to refusing on any local change and checking out the whole tree: {}",
                    self.config.name, e
                ));
                None
            }
        };
        self._refuse_if_update_would_overwrite(&fetch_commit, changed_paths.as_deref())?;
        let fetch_oid = fetch_commit.id();
        let remote_ref = match self._apply_fast_forward(fetch_commit, local_oid, changed_paths.as_deref()) {
            Ok(remote_ref) => remote_ref,
            Err(e) => {
                self._rollback_failed_apply(local_oid, fetch_oid);
                return Err(e);
            }
        };

        self._should_trigger_update(remote_ref, local_oid)
    }

    fn _refuse_if_update_would_overwrite(
        &self,
        fetch_commit: &AnnotatedCommit,
        changed_paths: Option<&[PathBuf]>,
    ) -> Result<(), ZlorbError> {
        let tracked_changes = self._tracked_local_changes()?;
        let tracked_changes: Vec<PathBuf> = match changed_paths {
            Some(paths) => tracked_changes
                .into_iter()
                .filter(|change| paths.contains(change))
                .collect(),
            None => tracked_changes,
        };
        let untracked_collisions = self._untracked_collisions(fetch_commit)?;
        if tracked_changes.is_empty() && untracked_collisions.is_empty() {
            return Ok(());
        }
        let mut reasons = Vec::new();
        if !tracked_changes.is_empty() {
            reasons.push(format!(
                "local changes to tracked files ({})",
                display_paths(&tracked_changes)
            ));
        }
        if !untracked_collisions.is_empty() {
            reasons.push(format!(
                "untracked files in the way ({})",
                untracked_collisions.join(", ")
            ));
        }
        Err(ZlorbError::Other(format!(
            "Refusing to fast-forward {}: {} would be overwritten. Stash the work, move it to another branch, or discard it — do not commit it on this branch, the daemon can only fast-forward this branch",
            self.config.name,
            reasons.join(" and ")
        )))
    }

    fn _untracked_collisions(
        &self,
        fetch_commit: &AnnotatedCommit,
    ) -> Result<Vec<String>, ZlorbError> {
        let Some(workdir) = self.repo.workdir().map(|path| path.to_path_buf()) else {
            return Ok(Vec::new());
        };
        let index = self.repo.index().map_err(ZlorbError::Git)?;
        let commit = self
            .repo
            .find_commit(fetch_commit.id())
            .map_err(ZlorbError::Git)?;
        let tree = commit.tree().map_err(ZlorbError::Git)?;
        let mut head_paths: std::collections::HashSet<Vec<u8>> =
            std::collections::HashSet::new();
        if let Ok(head) = self.repo.head() {
            if let Ok(head_commit) = head.peel_to_commit() {
                if let Ok(head_tree) = head_commit.tree() {
                    let _ = head_tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
                        if matches!(
                            entry.kind(),
                            Some(git2::ObjectType::Blob) | Some(git2::ObjectType::Commit)
                        ) {
                            let path =
                                PathBuf::from(root).join(entry.name().unwrap_or_default());
                            {
                                use std::os::unix::ffi::OsStrExt;
                                head_paths
                                    .insert(path.as_os_str().as_bytes().to_vec());
                            }
                        }
                        git2::TreeWalkResult::Ok
                    });
                }
            }
        }
        let mut typechange_cache: HashMap<PathBuf, bool> = HashMap::new();
        let mut collisions: Vec<String> = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if matches!(
                entry.kind(),
                Some(git2::ObjectType::Blob) | Some(git2::ObjectType::Commit)
            ) {
                let path = PathBuf::from(root).join(entry.name().unwrap_or_default());
                let components: Vec<_> = path.components().collect();
                let mut prefix = PathBuf::new();
                for (position, component) in components.iter().enumerate() {
                    prefix.push(component);
                    let Ok(metadata) = std::fs::symlink_metadata(workdir.join(&prefix)) else {
                        break;
                    };
                    let is_full_path = position + 1 == components.len();
                    let blocks_descent = !is_full_path && !metadata.is_dir();
                    if (is_full_path || blocks_descent)
                        && index.get_path(&prefix, 0).is_none()
                    {
                        let tracked_typechange = metadata.is_dir()
                            && *typechange_cache.entry(prefix.clone()).or_insert_with(|| {
                                directory_is_tracked_typechange(
                                    &head_paths, &workdir, &prefix,
                                )
                            });
                        if tracked_typechange {
                            break;
                        }
                        let label = prefix.to_string_lossy().into_owned();
                        if !collisions.contains(&label) {
                            collisions.push(label);
                        }
                        break;
                    }
                }
            }
            git2::TreeWalkResult::Ok
        })
        .map_err(ZlorbError::Git)?;
        Ok(collisions)
    }

    fn _tracked_local_changes(&self) -> Result<Vec<PathBuf>, ZlorbError> {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(false);
        opts.include_ignored(false);
        let statuses = self
            .repo
            .statuses(Some(&mut opts))
            .map_err(ZlorbError::Git)?;
        let mut changes: Vec<PathBuf> = statuses
            .iter()
            .map(|entry| path_from_bytes(entry.path_bytes()))
            .collect();
        changes.extend(self._hidden_local_changes(&changes));
        Ok(changes)
    }

    fn _hidden_local_changes(&self, known: &[PathBuf]) -> Vec<PathBuf> {
        let Some(workdir) = self.repo.workdir().map(|path| path.to_path_buf()) else {
            return Vec::new();
        };
        let index = match self.repo.index() {
            Ok(index) => index,
            Err(e) => {
                Logger::error(format!(
                    "Could not read the index for {}; local changes under skip-worktree or assume-unchanged cannot be checked this cycle and would not be reported: {}",
                    self.config.name, e
                ));
                return Vec::new();
            }
        };
        let file_mode = self
            .repo
            .config()
            .and_then(|config| config.get_bool("core.fileMode"))
            .unwrap_or(true);
        let mut hidden = Vec::new();
        for entry in index.iter() {
            if entry.flags & 0x3000 != 0 {
                continue;
            }
            let skip_worktree =
                git2::IndexEntryExtendedFlag::from_bits_truncate(entry.flags_extended)
                    .is_skip_worktree();
            let assume_unchanged =
                git2::IndexEntryFlag::from_bits_truncate(entry.flags).is_valid();
            if !skip_worktree && !assume_unchanged {
                continue;
            }
            let path = path_from_bytes(&entry.path);
            if known.contains(&path) || hidden.contains(&path) {
                continue;
            }
            let full_path = workdir.join(&path);
            let metadata = match std::fs::symlink_metadata(&full_path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    if assume_unchanged {
                        hidden.push(path);
                    }
                    continue;
                }
            };
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                let Ok(target) = std::fs::read_link(&full_path) else {
                    continue;
                };
                let target_bytes = {
                    use std::os::unix::ffi::OsStrExt;
                    target.as_os_str().as_bytes().to_vec()
                };
                let Ok(disk_id) = Oid::hash_object(git2::ObjectType::Blob, &target_bytes)
                else {
                    continue;
                };
                if disk_id != entry.id {
                    hidden.push(path);
                }
                continue;
            }
            if !file_type.is_file() {
                if matches!(entry.mode, 0o100644 | 0o100755 | 0o120000) {
                    hidden.push(path);
                }
                continue;
            }
            if file_mode && matches!(entry.mode, 0o100644 | 0o100755) {
                use std::os::unix::fs::PermissionsExt;
                let workdir_mode = if metadata.permissions().mode() & 0o111 != 0 {
                    0o100755
                } else {
                    0o100644
                };
                if workdir_mode != entry.mode {
                    hidden.push(path);
                    continue;
                }
            }
            if stat_matches_index_entry(&entry, &metadata) {
                continue;
            }
            let mut writer = match self.repo.blob_writer(Some(&path)) {
                Ok(writer) => writer,
                Err(e) => {
                    Logger::error(format!(
                        "Could not check {} in {} for local changes under skip-worktree or assume-unchanged ({}); treating it as changed so the update is refused rather than risk discarding it",
                        path.display(),
                        self.config.name,
                        e
                    ));
                    hidden.push(path);
                    continue;
                }
            };
            let mut file = match std::fs::File::open(&full_path) {
                Ok(file) => file,
                Err(e) => {
                    Logger::error(format!(
                        "Could not read {} in {} to check it for local changes under skip-worktree or assume-unchanged ({}); treating it as changed so the update is refused rather than risk discarding it",
                        path.display(),
                        self.config.name,
                        e
                    ));
                    hidden.push(path);
                    continue;
                }
            };
            if let Err(e) = std::io::copy(&mut file, &mut writer) {
                Logger::error(format!(
                    "Could not read {} in {} to check it for local changes under skip-worktree or assume-unchanged ({}); treating it as changed so the update is refused rather than risk discarding it",
                    path.display(),
                    self.config.name,
                    e
                ));
                hidden.push(path);
                continue;
            }
            let disk_id = match writer.commit() {
                Ok(disk_id) => disk_id,
                Err(e) => {
                    Logger::error(format!(
                        "Could not hash {} in {} to check it for local changes under skip-worktree or assume-unchanged ({}); treating it as changed so the update is refused rather than risk discarding it",
                        path.display(),
                        self.config.name,
                        e
                    ));
                    hidden.push(path);
                    continue;
                }
            };
            if disk_id != entry.id {
                if !self._matches_after_external_clean_filter(
                    &workdir,
                    &path,
                    &full_path,
                    entry.id,
                ) {
                    hidden.push(path);
                }
            }
        }
        hidden
    }

    fn _matches_after_external_clean_filter(
        &self,
        workdir: &Path,
        path: &Path,
        full_path: &Path,
        expected: Oid,
    ) -> bool {
        self._matches_after_external_clean_filter_with_timeout(
            workdir,
            path,
            full_path,
            expected,
            std::time::Duration::from_secs(30),
        )
    }

    fn _matches_after_external_clean_filter_with_timeout(
        &self,
        workdir: &Path,
        path: &Path,
        full_path: &Path,
        expected: Oid,
        timeout: std::time::Duration,
    ) -> bool {
        let Some(filter_name) = self._external_clean_filter_name(path) else {
            return false;
        };
        Logger::info(format!(
            "Re-checking {} in {} through git hash-object so the external clean filter {} is applied",
            path.display(),
            self.config.name,
            filter_name
        ));
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(workdir)
            .arg("hash-object")
            .arg("--")
            .arg(full_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                Logger::error(format!(
                    "Could not run git hash-object for {} in {} to apply the external clean filter {} ({}); treating it as changed so the update is refused rather than risk discarding it",
                    path.display(),
                    self.config.name,
                    filter_name,
                    e
                ));
                return false;
            }
        };
        let pgid = child.id() as libc::pid_t;
        let mut drains = match (child.stdout.take(), child.stderr.take()) {
            (Some(stdout), Some(stderr)) => PipeDrains::new(stdout, stderr),
            _ => {
                kill_process_group(pgid);
                let _ = child.kill();
                let _ = child.wait();
                Logger::error(format!(
                    "git hash-object for {} in {} did not provide output pipes while applying the external clean filter {}; treating it as changed so the update is refused rather than risk discarding it",
                    path.display(),
                    self.config.name,
                    filter_name
                ));
                return false;
            }
        };
        let deadline = std::time::Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        kill_process_group(pgid);
                        let _ = child.wait();
                        Logger::error(format!(
                            "git hash-object for {} in {} did not finish within {} seconds while applying the external clean filter {}; treating it as changed so the update is refused rather than risk discarding it",
                            path.display(),
                            self.config.name,
                            timeout.as_secs(),
                            filter_name
                        ));
                        let drain_deadline = pipe_drain_deadline();
                        let _ = drains.finish(drain_deadline, pgid);
                        return false;
                    }
                    drains.poll_ready(std::time::Duration::from_millis(10));
                }
                Err(e) => {
                    kill_process_group(pgid);
                    let _ = child.kill();
                    let _ = child.wait();
                    Logger::error(format!(
                        "Could not wait for git hash-object for {} in {} to apply the external clean filter {} ({}); treating it as changed so the update is refused rather than risk discarding it",
                        path.display(),
                        self.config.name,
                        filter_name,
                        e
                    ));
                    let drain_deadline = pipe_drain_deadline();
                    let _ = drains.finish(drain_deadline, pgid);
                    return false;
                }
            }
        };
        let drain_deadline = pipe_drain_deadline();
        if !drains.finish(drain_deadline, pgid) {
            Logger::error(format!(
                "git hash-object for {} in {} left its output pipes open after exiting while applying the external clean filter {}; treating it as changed so the update is refused rather than risk discarding it",
                path.display(),
                self.config.name,
                filter_name
            ));
            return false;
        }
        let stdout = std::mem::take(&mut drains.stdout.buf);
        let stderr = std::mem::take(&mut drains.stderr.buf);
        if !status.success() {
            Logger::error(format!(
                "git hash-object for {} in {} failed ({}) while applying the external clean filter {}; treating it as changed so the update is refused rather than risk discarding it: {}",
                path.display(),
                self.config.name,
                status,
                filter_name,
                String::from_utf8_lossy(&stderr).trim()
            ));
            return false;
        }
        let text = String::from_utf8_lossy(&stdout);
        match Oid::from_str(text.trim()) {
            Ok(external_id) => {
                if external_id != expected {
                    Logger::warn(format!(
                        "Re-checking {} in {} through the external clean filter {} still produced a different hash than the index; treating it as changed",
                        path.display(),
                        self.config.name,
                        filter_name
                    ));
                }
                external_id == expected
            }
            Err(e) => {
                Logger::error(format!(
                    "Could not parse the git hash-object result for {} in {} ({}); treating it as changed so the update is refused rather than risk discarding it",
                    path.display(),
                    self.config.name,
                    e
                ));
                false
            }
        }
    }

    fn _external_clean_filter_name(&self, path: &Path) -> Option<String> {
        let value = self
            .repo
            .get_attr(path, "filter", git2::AttrCheckFlags::FILE_THEN_INDEX)
            .ok()?;
        let name = match git2::AttrValue::from_string(value) {
            git2::AttrValue::String(name) => name.to_string(),
            _ => return None,
        };
        let config = self.repo.config().ok()?;
        let configured = ["clean", "process"].iter().any(|kind| {
            config
                .get_string(&format!("filter.{}.{}", name, kind))
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
        });
        if configured {
            Some(name)
        } else {
            None
        }
    }

    fn _apply_fast_forward(
        &self,
        fetch_commit: AnnotatedCommit,
        local_oid: Oid,
        changed_paths: Option<&[PathBuf]>,
    ) -> Result<Reference<'_>, ZlorbError> {
        self._refuse_if_update_would_overwrite(&fetch_commit, changed_paths)?;
        let fetch_oid = fetch_commit.id();
        self._setup_repo_for_checkout(fetch_commit)?;
        self._checkout_and_get_ref(Some(local_oid), fetch_oid, changed_paths)
    }

    fn _changed_paths(&self, from: Oid, to: Oid) -> Result<Vec<PathBuf>, ZlorbError> {
        let from_tree = self
            .repo
            .find_commit(from)
            .map_err(ZlorbError::Git)?
            .tree()
            .map_err(ZlorbError::Git)?;
        let to_tree = self
            .repo
            .find_commit(to)
            .map_err(ZlorbError::Git)?
            .tree()
            .map_err(ZlorbError::Git)?;
        let diff = self
            .repo
            .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), None)
            .map_err(ZlorbError::Git)?;
        let mut seen = std::collections::HashSet::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        for delta in diff.deltas() {
            for file in [delta.old_file(), delta.new_file()] {
                if let Some(path) = file.path() {
                    let path = path.to_path_buf();
                    if seen.insert(path.clone()) {
                        paths.push(path);
                    }
                }
            }
        }
        Ok(paths)
    }

    fn _rollback_failed_apply(&self, local_oid: Oid, fetch_oid: Oid) -> bool {
        let refname = format!("refs/heads/{}", self.config.branch);
        let branch_at_local = matches!(
            self.repo.find_reference(&refname),
            Ok(reference) if reference.target() == Some(local_oid)
        );
        if !branch_at_local {
            return self._rollback_fast_forward(local_oid, fetch_oid);
        }
        Logger::info(format!(
            "The failed update for {} never moved {} to the fetched commit {}, so the daemon wrote nothing to the working tree in this attempt; skipping the rollback cleanup and restore so files the daemon did not write are preserved",
            self.config.name, refname, fetch_oid
        ));
        self._verify_rollback(local_oid, fetch_oid)
    }

    fn _rollback_fast_forward(&self, local_oid: Oid, fetch_oid: Oid) -> bool {
        let refname = format!("refs/heads/{}", self.config.branch);
        let mut branch_restored = false;
        match self.repo.find_reference(&refname) {
            Ok(mut reference) => {
                match reference.set_target(local_oid, "Rolling back failed fast-forward") {
                    Ok(_) => branch_restored = true,
                    Err(e) => {
                        Logger::error(format!(
                            "Failed to roll back {} to {}: {}",
                            refname, local_oid, e
                        ));
                    }
                }
            }
            Err(e) => {
                Logger::error(format!(
                    "Failed to find {} to roll back {} after a failed fast-forward: {}",
                    refname, self.config.name, e
                ));
            }
        }
        self._remove_partial_update_files(local_oid, fetch_oid);
        let changed_paths = match self._changed_paths(local_oid, fetch_oid) {
            Ok(paths) => Some(paths),
            Err(e) => {
                Logger::error(format!(
                    "Could not compute the paths changed by the update for {}; rolling back with a scoped fallback restore, which may discard local changes outside the update: {}",
                    self.config.name, e
                ));
                None
            }
        };
        let mut whole_tree_refused = false;
        let (restore_paths, preserved_paths) = match &changed_paths {
            Some(paths) => {
                let (safe, preserved, classification_failed) =
                    self._rollback_restore_paths(local_oid, fetch_oid, paths);
                for path in &preserved {
                    if classification_failed {
                        Logger::error(format!(
                            "Preserving {} in {} during rollback: the changed paths could not be classified because a commit, tree, or the working directory could not be loaded, so it is not being restored",
                            path.display(),
                            self.config.name
                        ));
                    } else {
                        Logger::error(format!(
                            "Preserving local changes to {} in {} during rollback: the file changed after the update was checked and matches neither the rolled-back commit nor the fetched commit, so it is not being restored and the working tree is left diverged from {}",
                            path.display(),
                            self.config.name,
                            local_oid
                        ));
                    }
                }
                (Some(safe), preserved)
            }
            None => match self._rollback_scoped_fallback_paths(local_oid, fetch_oid) {
                Some((safe, preserved)) => {
                    for path in &preserved {
                        Logger::error(format!(
                            "Preserving local changes to {} in {} during rollback: the path could not be certified as matching the rolled-back commit or the fetched commit, or it sits under a symlinked ancestor, so the scoped fallback restore is not writing to it and the working tree is left diverged from {}",
                            path.display(),
                            self.config.name,
                            local_oid
                        ));
                    }
                    (Some(safe), preserved)
                }
                None => {
                    whole_tree_refused = true;
                    Logger::error(format!(
                        "Refusing the fallback restore for {}: the paths changed by the update could not be computed and the rolled-back and fetched trees could not be enumerated completely, so no working-tree restore is being attempted and the working tree is left diverged from {}",
                        self.config.name, local_oid
                    ));
                    (Some(Vec::new()), Vec::new())
                }
            },
        };
        let mut tree_restored = false;
        let restore_result = match self.repo.find_commit(local_oid) {
            Ok(commit) => {
                if matches!(&restore_paths, Some(paths) if paths.is_empty()) {
                    Ok(())
                } else {
                    let mut checkout = git2::build::CheckoutBuilder::default();
                    checkout.force();
                    checkout.disable_pathspec_match(true);
                    if let Some(paths) = &restore_paths {
                        for path in paths {
                            checkout.path(path);
                        }
                    }
                    self.repo
                        .checkout_tree(commit.as_object(), Some(&mut checkout))
                }
            }
            Err(e) => Err(e),
        };
        match restore_result {
            Ok(_) => tree_restored = preserved_paths.is_empty(),
            Err(e) => {
                Logger::error(format!(
                    "Failed to restore the working tree after a failed fast-forward: {}",
                    e
                ));
            }
        }
        if whole_tree_refused {
            tree_restored = false;
        }
        let mut head_restored = !branch_restored;
        if branch_restored {
            match self.repo.set_head(&refname) {
                Ok(_) => head_restored = true,
                Err(e) => {
                    Logger::error(format!(
                        "Failed to point HEAD at {} after rolling back a failed fast-forward: {}",
                        refname, e
                    ));
                }
            }
        }
        let verified = self._verify_rollback(local_oid, fetch_oid);
        if verified && !(tree_restored && head_restored) {
            Logger::error(format!(
                "Rollback verification failed for {}: {} is back at {}, but the working tree or HEAD could not be restored to match, so the rolled-back state does not hold and this update will not be built from it without manual intervention",
                self.config.name, refname, local_oid
            ));
            return false;
        }
        verified
    }

    fn _checkout_dir_is_safe(&self, local_oid: Oid, fetch_oid: Oid, path: &Path) -> bool {
        let Some(workdir) = self.repo.workdir().map(|workdir| workdir.to_path_buf()) else {
            return false;
        };
        if self._rollback_has_symlinked_ancestor(&workdir, path) {
            return false;
        }
        let full_path = workdir.join(path);
        let is_real_dir = std::fs::symlink_metadata(&full_path)
            .map(|metadata| metadata.file_type().is_dir())
            .unwrap_or(false);
        if !is_real_dir {
            return false;
        }
        for oid in [local_oid, fetch_oid] {
            let Ok(commit) = self.repo.find_commit(oid) else {
                continue;
            };
            let Ok(tree) = commit.tree() else {
                continue;
            };
            let Ok(entry) = tree.get_path(path) else {
                continue;
            };
            if entry.filemode() != 0o40000 {
                continue;
            }
            let Ok(subtree) = self.repo.find_tree(entry.id()) else {
                continue;
            };
            if self._checkout_dir_matches_tree(&workdir, path, &subtree) {
                return true;
            }
        }
        false
    }

    fn _checkout_dir_matches_tree(
        &self,
        workdir: &Path,
        path: &Path,
        tree: &git2::Tree,
    ) -> bool {
        let full_path = workdir.join(path);
        let Ok(read_dir) = std::fs::read_dir(&full_path) else {
            return false;
        };
        let mut disk_names = std::collections::HashSet::new();
        for entry in read_dir {
            let Ok(entry) = entry else {
                return false;
            };
            disk_names.insert(entry.file_name());
        }
        let mut tree_names = std::collections::HashSet::new();
        for entry in tree.iter() {
            let Some(name) = entry.name() else {
                return false;
            };
            tree_names.insert(std::ffi::OsString::from(name));
            if !disk_names.contains(std::ffi::OsStr::new(name)) {
                return false;
            }
            let child_path = path.join(name);
            let child_full = workdir.join(&child_path);
            let child_is_real_dir = std::fs::symlink_metadata(&child_full)
                .map(|metadata| metadata.file_type().is_dir())
                .unwrap_or(false);
            let matches = match entry.kind() {
                Some(git2::ObjectType::Tree) => {
                    child_is_real_dir
                        && match entry.to_object(&self.repo) {
                            Ok(object) => match object.as_tree() {
                                Some(child_tree) => self._checkout_dir_matches_tree(
                                    workdir,
                                    &child_path,
                                    child_tree,
                                ),
                                None => false,
                            },
                            Err(_) => false,
                        }
                }
                Some(git2::ObjectType::Commit) => {
                    child_is_real_dir
                        && self._rollback_gitlink_matches(&child_full, entry.id(), true)
                }
                _ => self._rollback_disk_matches(
                    workdir,
                    &child_path,
                    Some(entry.id()),
                    Some(entry.filemode()),
                ),
            };
            if !matches {
                return false;
            }
        }
        for name in &disk_names {
            if !tree_names.contains(name)
                && !Self::_rollback_dir_holds_no_files(&full_path.join(name))
                && !self._checkout_extra_entry_holds_only_ignored(&path.join(name))
            {
                return false;
            }
        }
        true
    }

    fn _checkout_extra_entry_holds_only_ignored(&self, rel_path: &Path) -> bool {
        let Ok(mut index) = self.repo.index() else {
            return false;
        };
        if index.read(true).is_err() {
            return false;
        }
        if index
            .iter()
            .any(|entry| path_from_bytes(&entry.path).starts_with(rel_path))
        {
            return false;
        }
        self._checkout_extra_entry_holds_only_ignored_indexed(rel_path, &index)
    }

    fn _checkout_extra_entry_holds_only_ignored_indexed(
        &self,
        rel_path: &Path,
        index: &git2::Index,
    ) -> bool {
        if rel_path.file_name() == Some(std::ffi::OsStr::new(".git")) {
            return false;
        }
        if (0..=3).any(|stage| {
            matches!(index.get_path(rel_path, stage), Some(entry) if entry.mode == 0o160000)
        }) {
            return false;
        }
        let Some(workdir) = self.repo.workdir().map(|workdir| workdir.to_path_buf()) else {
            return false;
        };
        let full_path = workdir.join(rel_path);
        let Ok(metadata) = std::fs::symlink_metadata(&full_path) else {
            return false;
        };
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            if git2::Repository::open_ext(
                &full_path,
                git2::RepositoryOpenFlags::NO_SEARCH,
                Vec::<&Path>::new(),
            )
            .is_ok()
            {
                return false;
            }
            if full_path.join("HEAD").is_file()
                && full_path.join("objects").is_dir()
                && full_path.join("refs").is_dir()
            {
                return false;
            }
            let Ok(entries) = std::fs::read_dir(&full_path) else {
                return false;
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    return false;
                };
                if !self._checkout_extra_entry_holds_only_ignored_indexed(
                    &rel_path.join(entry.file_name()),
                    index,
                ) {
                    return false;
                }
            }
            true
        } else if file_type.is_file() {
            let in_index = (0..=3).any(|stage| index.get_path(rel_path, stage).is_some());
            !in_index && self.repo.is_path_ignored(rel_path).unwrap_or(false)
        } else {
            false
        }
    }

    fn _rollback_restore_paths(
        &self,
        local_oid: Oid,
        fetch_oid: Oid,
        changed_paths: &[PathBuf],
    ) -> (Vec<PathBuf>, Vec<PathBuf>, bool) {
        let mut load_failed = false;
        let local_commit = match self.repo.find_commit(local_oid) {
            Ok(commit) => Some(commit),
            Err(e) => {
                load_failed = true;
                Logger::error(format!(
                    "Could not load the current commit {} for {} to classify the changed paths, so every changed path is being preserved and none will be overwritten: {}",
                    local_oid, self.config.name, e
                ));
                None
            }
        };
        let fetch_commit = match self.repo.find_commit(fetch_oid) {
            Ok(commit) => Some(commit),
            Err(e) => {
                load_failed = true;
                Logger::error(format!(
                    "Could not load the fetched commit {} for {} to classify the changed paths, so every changed path is being preserved and none will be overwritten: {}",
                    fetch_oid, self.config.name, e
                ));
                None
            }
        };
        if load_failed {
            return (Vec::new(), changed_paths.to_vec(), true);
        }
        let local_commit = local_commit.unwrap();
        let fetch_commit = fetch_commit.unwrap();
        let local_tree = match local_commit.tree() {
            Ok(tree) => Some(tree),
            Err(e) => {
                load_failed = true;
                Logger::error(format!(
                    "Could not load the tree of the current commit {} for {} to classify the changed paths, so every changed path is being preserved and none will be overwritten: {}",
                    local_oid, self.config.name, e
                ));
                None
            }
        };
        let fetch_tree = match fetch_commit.tree() {
            Ok(tree) => Some(tree),
            Err(e) => {
                load_failed = true;
                Logger::error(format!(
                    "Could not load the tree of the fetched commit {} for {} to classify the changed paths, so every changed path is being preserved and none will be overwritten: {}",
                    fetch_oid, self.config.name, e
                ));
                None
            }
        };
        if load_failed {
            return (Vec::new(), changed_paths.to_vec(), true);
        }
        let local_tree = local_tree.unwrap();
        let fetch_tree = fetch_tree.unwrap();
        let Some(workdir) = self.repo.workdir().map(|path| path.to_path_buf()) else {
            Logger::error(format!(
                "{} has no working directory to classify the changed paths against, so every changed path is being preserved and none will be overwritten",
                self.config.name
            ));
            return (Vec::new(), changed_paths.to_vec(), true);
        };
        let mut safe = Vec::new();
        let mut preserved = Vec::new();
        for path in changed_paths {
            if self._rollback_has_symlinked_ancestor(&workdir, path) {
                preserved.push(path.clone());
                continue;
            }
            let local_entry = local_tree.get_path(path).ok();
            let fetch_entry = fetch_tree.get_path(path).ok();
            let local_id = local_entry.as_ref().map(|entry| entry.id());
            let fetch_id = fetch_entry.as_ref().map(|entry| entry.id());
            let local_mode = local_entry.as_ref().map(|entry| entry.filemode());
            let fetch_mode = fetch_entry.as_ref().map(|entry| entry.filemode());
            if std::fs::symlink_metadata(workdir.join(path)).is_err() {
                if local_id.is_some() && fetch_id.is_some() {
                    preserved.push(path.clone());
                } else {
                    safe.push(path.clone());
                }
                continue;
            }
            if self._rollback_disk_matches(&workdir, path, local_id, local_mode)
                || self._rollback_disk_matches(&workdir, path, fetch_id, fetch_mode)
                || self._rollback_structured_matches(
                    &workdir, path, local_id, local_mode, fetch_id, fetch_mode,
                )
            {
                safe.push(path.clone());
            } else {
                preserved.push(path.clone());
            }
        }
        (safe, preserved, false)
    }

    fn _rollback_scoped_fallback_paths(
        &self,
        local_oid: Oid,
        fetch_oid: Oid,
    ) -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
        let workdir = self.repo.workdir()?.to_path_buf();
        let local_tree = self.repo.find_commit(local_oid).ok()?.tree().ok()?;
        let fetch_tree = self.repo.find_commit(fetch_oid).ok()?.tree().ok()?;
        let mut seen = std::collections::HashSet::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        for tree in [&local_tree, &fetch_tree] {
            let mut tree_paths: Vec<PathBuf> = Vec::new();
            tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
                if matches!(
                    entry.kind(),
                    Some(git2::ObjectType::Blob) | Some(git2::ObjectType::Commit)
                ) {
                    if let Some(name) = entry.name() {
                        if !name.is_empty() {
                            tree_paths.push(PathBuf::from(root).join(name));
                        }
                    }
                }
                git2::TreeWalkResult::Ok
            })
            .ok()?;
            for path in tree_paths {
                if seen.insert(path.clone()) {
                    paths.push(path);
                }
            }
        }
        if paths.is_empty() {
            return None;
        }
        let mut safe = Vec::new();
        let mut preserved = Vec::new();
        for path in paths {
            if self._rollback_has_symlinked_ancestor(&workdir, &path) {
                preserved.push(path);
                continue;
            }
            let local_entry = local_tree.get_path(&path).ok();
            let fetch_entry = fetch_tree.get_path(&path).ok();
            let local_id = local_entry.as_ref().map(|entry| entry.id());
            let fetch_id = fetch_entry.as_ref().map(|entry| entry.id());
            let local_mode = local_entry.as_ref().map(|entry| entry.filemode());
            let fetch_mode = fetch_entry.as_ref().map(|entry| entry.filemode());
            if std::fs::symlink_metadata(workdir.join(&path)).is_err() {
                if local_id.is_some() && fetch_id.is_some() {
                    preserved.push(path);
                } else {
                    safe.push(path);
                }
                continue;
            }
            if self._rollback_disk_matches(&workdir, &path, local_id, local_mode)
                || self._rollback_disk_matches(&workdir, &path, fetch_id, fetch_mode)
                || self._rollback_structured_matches(
                    &workdir, &path, local_id, local_mode, fetch_id, fetch_mode,
                )
            {
                safe.push(path);
            } else {
                preserved.push(path);
            }
        }
        Some((safe, preserved))
    }

    fn _rollback_has_symlinked_ancestor(&self, workdir: &Path, path: &Path) -> bool {
        let components: Vec<_> = path.components().collect();
        let mut prefix = PathBuf::new();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            prefix.push(component);
            let is_symlink = std::fs::symlink_metadata(workdir.join(&prefix))
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false);
            if is_symlink {
                return true;
            }
        }
        false
    }

    fn _rollback_disk_matches(
        &self,
        workdir: &Path,
        path: &Path,
        entry_id: Option<Oid>,
        entry_mode: Option<i32>,
    ) -> bool {
        let Some(expected) = entry_id else {
            return false;
        };
        let full_path = workdir.join(path);
        let Ok(metadata) = std::fs::symlink_metadata(&full_path) else {
            return false;
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            if entry_mode != Some(0o120000) {
                return false;
            }
            let Ok(target) = std::fs::read_link(&full_path) else {
                return false;
            };
            let target_bytes = {
                use std::os::unix::ffi::OsStrExt;
                target.as_os_str().as_bytes().to_vec()
            };
            return Oid::hash_object(git2::ObjectType::Blob, &target_bytes).ok()
                == Some(expected);
        }
        if !file_type.is_file() {
            return false;
        }
        if !matches!(entry_mode, Some(0o100644) | Some(0o100755)) {
            return false;
        }
        let file_mode = self
            .repo
            .config()
            .and_then(|config| config.get_bool("core.fileMode"))
            .unwrap_or(true);
        if file_mode {
            use std::os::unix::fs::PermissionsExt;
            let disk_mode = if metadata.permissions().mode() & 0o111 != 0 {
                0o100755
            } else {
                0o100644
            };
            if Some(disk_mode) != entry_mode {
                return false;
            }
        }
        if let Ok(contents) = std::fs::read(&full_path) {
            if Oid::hash_object(git2::ObjectType::Blob, &contents).ok() == Some(expected)
            {
                return true;
            }
        }
        if self._rollback_filtered_disk_id(path, &full_path) == Some(expected) {
            return true;
        }
        self._matches_after_external_clean_filter(workdir, path, &full_path, expected)
    }

    fn _rollback_structured_matches(
        &self,
        workdir: &Path,
        path: &Path,
        local_id: Option<Oid>,
        local_mode: Option<i32>,
        fetch_id: Option<Oid>,
        fetch_mode: Option<i32>,
    ) -> bool {
        let full_path = workdir.join(path);
        let Ok(metadata) = std::fs::symlink_metadata(&full_path) else {
            return false;
        };
        if !metadata.file_type().is_dir() {
            return false;
        }
        for (entry_id, entry_mode) in [(local_id, local_mode), (fetch_id, fetch_mode)] {
            let Some(expected) = entry_id else {
                continue;
            };
            let both_gitlinks =
                local_mode == Some(0o160000) && fetch_mode == Some(0o160000);
            let matches = match entry_mode {
                Some(0o160000) => {
                    self._rollback_gitlink_matches(&full_path, expected, !both_gitlinks)
                }
                Some(0o40000) => match self.repo.find_tree(expected) {
                    Ok(tree) => self._rollback_dir_matches_tree(workdir, path, &tree),
                    Err(_) => false,
                },
                _ => false,
            };
            if matches {
                return true;
            }
        }
        false
    }

    fn _rollback_gitlink_matches(
        &self,
        full_path: &Path,
        expected: Oid,
        include_ignored: bool,
    ) -> bool {
        match git2::Repository::open(full_path) {
            Ok(sub_repo) => {
                let Some(head) = sub_repo.head().ok().and_then(|head| head.target()) else {
                    return self
                        ._rollback_gitlink_is_submodule_placeholder(&sub_repo, full_path);
                };
                if head != expected {
                    return false;
                }
                if sub_repo.is_bare() {
                    return false;
                }
                let mut status_options = git2::StatusOptions::new();
                status_options.include_untracked(true);
                status_options.include_ignored(include_ignored);
                matches!(
                    sub_repo.statuses(Some(&mut status_options)),
                    Ok(statuses) if statuses.is_empty()
                )
            }
            Err(_) => match std::fs::read_dir(full_path) {
                Ok(mut entries) => entries.next().is_none(),
                Err(_) => false,
            },
        }
    }

    fn _rollback_gitlink_is_submodule_placeholder(
        &self,
        sub_repo: &git2::Repository,
        full_path: &Path,
    ) -> bool {
        if sub_repo.is_bare() {
            return false;
        }
        let workdir_matches = match (sub_repo.workdir(), std::fs::canonicalize(full_path)) {
            (Some(workdir), Ok(path)) => {
                std::fs::canonicalize(workdir).map(|dir| dir == path).unwrap_or(false)
            }
            _ => false,
        };
        if !workdir_matches {
            return false;
        }
        if !matches!(sub_repo.index(), Ok(index) if index.is_empty()) {
            return false;
        }
        let Ok(gitdir) = std::fs::canonicalize(sub_repo.path()) else {
            return false;
        };
        let Ok(modules_dir) = std::fs::canonicalize(self.repo.path().join("modules")) else {
            return false;
        };
        if gitdir == modules_dir || !gitdir.starts_with(&modules_dir) {
            return false;
        }
        let Ok(odb) = sub_repo.odb() else {
            return false;
        };
        let object_count = std::cell::Cell::new(0usize);
        let counted = odb
            .foreach(|_| {
                object_count.set(object_count.get() + 1);
                true
            })
            .is_ok();
        if !counted || object_count.get() != 0 {
            return false;
        }
        let mut status_options = git2::StatusOptions::new();
        status_options.include_untracked(true);
        status_options.include_ignored(true);
        let Ok(statuses) = sub_repo.statuses(Some(&mut status_options)) else {
            return false;
        };
        statuses.iter().all(|entry| {
            entry.status() == git2::Status::IGNORED
                && entry
                    .path()
                    .is_some_and(|path| Self::_rollback_dir_holds_no_files(&full_path.join(path)))
        })
    }

    fn _rollback_dir_holds_no_files(dir: &Path) -> bool {
        if !std::fs::symlink_metadata(dir)
            .map(|metadata| metadata.file_type().is_dir())
            .unwrap_or(false)
        {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
                return false;
            };
            if metadata.file_type().is_dir() {
                if !Self::_rollback_dir_holds_no_files(&entry.path()) {
                    return false;
                }
            } else {
                return false;
            }
        }
        true
    }

    fn _rollback_dir_matches_tree(
        &self,
        workdir: &Path,
        path: &Path,
        tree: &git2::Tree,
    ) -> bool {
        let full_path = workdir.join(path);
        let Ok(read_dir) = std::fs::read_dir(&full_path) else {
            return false;
        };
        let mut disk_names = std::collections::HashSet::new();
        for entry in read_dir {
            let Ok(entry) = entry else {
                return false;
            };
            disk_names.insert(entry.file_name());
        }
        if disk_names.len() != tree.len() {
            return false;
        }
        for entry in tree.iter() {
            let Some(name) = entry.name() else {
                return false;
            };
            if !disk_names.contains(std::ffi::OsStr::new(name)) {
                return false;
            }
            let child_path = path.join(name);
            let child_full = workdir.join(&child_path);
            let child_is_real_dir = std::fs::symlink_metadata(&child_full)
                .map(|metadata| metadata.file_type().is_dir())
                .unwrap_or(false);
            let matches = match entry.kind() {
                Some(git2::ObjectType::Tree) => {
                    child_is_real_dir
                        && match entry.to_object(&self.repo) {
                            Ok(object) => match object.as_tree() {
                                Some(child_tree) => self._rollback_dir_matches_tree(
                                    workdir,
                                    &child_path,
                                    child_tree,
                                ),
                                None => false,
                            },
                            Err(_) => false,
                        }
                }
                Some(git2::ObjectType::Commit) => {
                    child_is_real_dir
                        && self._rollback_gitlink_matches(&child_full, entry.id(), true)
                }
                _ => self._rollback_disk_matches(
                    workdir,
                    &child_path,
                    Some(entry.id()),
                    Some(entry.filemode()),
                ),
            };
            if !matches {
                return false;
            }
        }
        true
    }

    fn _rollback_filtered_disk_id(&self, path: &Path, full_path: &Path) -> Option<Oid> {
        let mut writer = self.repo.blob_writer(Some(path)).ok()?;
        let mut file = std::fs::File::open(full_path).ok()?;
        std::io::copy(&mut file, &mut writer).ok()?;
        writer.commit().ok()
    }

    fn _verify_rollback(&self, local_oid: Oid, fetch_oid: Oid) -> bool {
        let refname = format!("refs/heads/{}", self.config.branch);
        let reference = match self.repo.find_reference(&refname) {
            Ok(reference) => reference,
            Err(e) => {
                Logger::error(format!(
                    "Rollback verification failed for {}: {} could not be read after rolling back a failed fast-forward ({}), expected {}; the repository state is unknown and this update will not be retried or built without manual intervention",
                    self.config.name, refname, e, local_oid
                ));
                return false;
            }
        };
        match reference.target() {
            Some(oid) if oid == local_oid => true,
            Some(oid) if oid == fetch_oid => {
                let head_detached_at_local = self.repo.head_detached().unwrap_or(false)
                    && self
                        .repo
                        .head()
                        .ok()
                        .and_then(|head| head.target())
                        == Some(local_oid);
                if head_detached_at_local {
                    Logger::error(format!(
                        "Rollback verification failed for {}: {} is {} after rolling back a failed fast-forward, expected {}; the branch is left at the fetched commit {}, but HEAD is detached at {}, so later cycles compare against the detached HEAD and may retry the update while the branch itself will not move without manual intervention",
                        self.config.name, refname, oid, local_oid, fetch_oid, local_oid
                    ));
                } else {
                    Logger::error(format!(
                        "Rollback verification failed for {}: {} is {} after rolling back a failed fast-forward, expected {}; the branch and HEAD are left at the fetched commit {} and later cycles will read it as up to date, so this update will not be retried or built without manual intervention",
                        self.config.name, refname, oid, local_oid, fetch_oid
                    ));
                }
                false
            }
            Some(oid) => {
                Logger::error(format!(
                    "Rollback verification failed for {}: {} is {} after rolling back a failed fast-forward, expected {}; it is not the fetched commit {}, so the rolled-back state does not hold and the next cycle will re-read the branch against the remote",
                    self.config.name, refname, oid, local_oid, fetch_oid
                ));
                false
            }
            None => {
                Logger::error(format!(
                    "Rollback verification failed for {}: {} has no target after rolling back a failed fast-forward, expected {}; the repository state is unknown and this update will not be retried or built without manual intervention",
                    self.config.name, refname, local_oid
                ));
                false
            }
        }
    }

    fn _remove_partial_update_files(&self, local_oid: Oid, fetch_oid: Oid) {
        let (Ok(fetch_commit), Ok(local_commit)) = (
            self.repo.find_commit(fetch_oid),
            self.repo.find_commit(local_oid),
        ) else {
            return;
        };
        let (Ok(fetch_tree), Ok(local_tree)) = (fetch_commit.tree(), local_commit.tree())
        else {
            return;
        };
        let Some(workdir) = self.repo.workdir().map(|path| path.to_path_buf()) else {
            return;
        };
        let Ok(index) = self.repo.index() else {
            return;
        };
        let mut added: Vec<(PathBuf, Oid)> = Vec::new();
        let _ = fetch_tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if matches!(
                entry.kind(),
                Some(git2::ObjectType::Blob) | Some(git2::ObjectType::Commit)
            ) {
                let path = PathBuf::from(root).join(entry.name().unwrap_or_default());
                if local_tree.get_path(&path).is_err() {
                    added.push((path, entry.id()));
                }
            }
            git2::TreeWalkResult::Ok
        });
        for (path, entry_id) in added {
            if self._rollback_has_symlinked_ancestor(&workdir, &path) {
                continue;
            }
            let full_path = workdir.join(&path);
            if index.get_path(&path, 0).is_some() {
                continue;
            }
            let Ok(metadata) = std::fs::symlink_metadata(&full_path) else {
                continue;
            };
            let file_type = metadata.file_type();
            let contents = if file_type.is_symlink() {
                let Ok(target) = std::fs::read_link(&full_path) else {
                    continue;
                };
                {
                    use std::os::unix::ffi::OsStrExt;
                    target.as_os_str().as_bytes().to_vec()
                }
            } else if file_type.is_file() {
                let Ok(contents) = std::fs::read(&full_path) else {
                    continue;
                };
                contents
            } else {
                continue;
            };
            let Ok(disk_id) = Oid::hash_object(git2::ObjectType::Blob, &contents) else {
                continue;
            };
            if disk_id != entry_id {
                continue;
            }
            if let Err(e) = std::fs::remove_file(&full_path) {
                Logger::error(format!(
                    "Failed to remove {} left by a failed fast-forward: {}",
                    full_path.display(),
                    e
                ));
            }
        }
    }

    fn _setup_fetchoptions_with_creds(&self) -> FetchOptions<'_> {
        let mut callbacks = RemoteCallbacks::new();
        //TODO: Complete the credential stuff
        callbacks.credentials(|_, _, _| Cred::userpass_plaintext("", ""));
        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);
        fetch_options
    }

    fn _get_local_branch(&self) -> Result<Branch<'_>, ZlorbError> {
        let branch = self
            .repo
            .find_branch(&self.config.branch, BranchType::Local)
            .map_err(|e| ZlorbError::Other(e.to_string()))?;
        Ok(branch)
    }

    fn _get_local_oid_from_branch(&self, branch: Branch) -> Result<Oid, ZlorbError> {
        let id = branch.get().target();
        if id.is_none() {
            return Err(ZlorbError::Git(git2::Error::new(
                git2::ErrorCode::NotFound,
                git2::ErrorClass::None,
                "Failed to get Oid from branch",
            )));
        };
        Ok(id.unwrap())
    }

    fn _get_remote(&self) -> Result<Remote<'_>, ZlorbError> {
        let remote = self
            .repo
            .find_remote(&self.config.remote)
            .map_err(ZlorbError::Git)?;
        Ok(remote)
    }

    fn _download_new_data(&self, remote: &mut Remote) {
        let _ = remote.fetch(
            std::slice::from_ref(&self.config.branch),
            Some(&mut self._setup_fetchoptions_with_creds()),
            None,
        );
    }

    fn _get_analysis(
        &self,
    ) -> Result<(MergeAnalysis, MergePreference, AnnotatedCommit<'_>), ZlorbError> {
        let r = &self.repo;
        let fetch_head = r.find_reference("FETCH_HEAD").map_err(ZlorbError::Git)?;
        let fetch_commit = r
            .reference_to_annotated_commit(&fetch_head)
            .map_err(ZlorbError::Git)?;
        let analysis = r
            .merge_analysis(&[&fetch_commit])
            .map_err(ZlorbError::Git)?;
        let (m, n) = analysis;
        Ok((m, n, fetch_commit))
    }

    fn _setup_repo_for_checkout(&self, fetch_commit: AnnotatedCommit) -> Result<(), ZlorbError> {
        let refname = format!("refs/heads/{}", self.config.branch);
        self.repo
            .find_reference(&refname)
            .map_err(ZlorbError::Git)?
            .set_target(fetch_commit.id(), "Fast-Forward")
            .map_err(ZlorbError::Git)?;
        self.repo.set_head(&refname).map_err(ZlorbError::Git)?;
        Ok(())
    }

    fn _checkout_and_get_ref(
        &self,
        local_oid: Option<Oid>,
        fetch_oid: Oid,
        changed_paths: Option<&[PathBuf]>,
    ) -> Result<Reference<'_>, ZlorbError> {
        let skip_checkout = matches!(changed_paths, Some(paths) if paths.is_empty());
        if !skip_checkout {
            let mut checkout = git2::build::CheckoutBuilder::default();
            checkout.force();
            checkout.disable_pathspec_match(true);
            if let Some(paths) = changed_paths {
                let paths = match local_oid {
                    Some(local_oid) => {
                        let (mut safe, classified, classification_failed) =
                            self._rollback_restore_paths(local_oid, fetch_oid, paths);
                        let mut preserved = Vec::new();
                        for path in classified {
                            if !classification_failed
                                && self._checkout_dir_is_safe(local_oid, fetch_oid, &path)
                            {
                                safe.push(path);
                            } else {
                                preserved.push(path);
                            }
                        }
                        if !preserved.is_empty() {
                            for path in &preserved {
                                if classification_failed {
                                    Logger::error(format!(
                                        "Preserving {} in {} at checkout: the changed paths could not be classified because a commit, tree, or the working directory could not be loaded, so it is not being overwritten and the update is refused",
                                        path.display(),
                                        self.config.name
                                    ));
                                } else {
                                    Logger::error(format!(
                                        "Preserving local changes to {} in {} at checkout: the file changed after the update was checked and matches neither the current commit nor the fetched commit, so it is not being overwritten and the update is refused",
                                        path.display(),
                                        self.config.name
                                    ));
                                }
                            }
                            if classification_failed {
                                return Err(ZlorbError::Other(format!(
                                    "Refusing to fast-forward {}: the changed paths ({}) could not be classified because a commit, tree, or the working directory could not be loaded, so no path is being overwritten by the checkout",
                                    self.config.name,
                                    display_paths(&preserved)
                                )));
                            }
                            return Err(ZlorbError::Other(format!(
                                "Refusing to fast-forward {}: local changes to tracked files ({}) would be overwritten by the checkout. Stash the work, move it to another branch, or discard it — do not commit it on this branch, the daemon can only fast-forward this branch",
                                self.config.name,
                                display_paths(&preserved)
                            )));
                        }
                        safe
                    }
                    None => {
                        return Err(ZlorbError::Other(format!(
                            "Refusing to fast-forward {}: the current commit could not be determined, so the checkout cannot be checked against it and no path is being overwritten",
                            self.config.name
                        )));
                    }
                };
                for path in &paths {
                    checkout.path(path);
                }
            }
            self.repo
                .checkout_head(Some(&mut checkout))
                .map_err(ZlorbError::Git)?;
        }

        let remote_ref = self
            .repo
            .resolve_reference_from_short_name(&format!(
                "{}/{}",
                self.config.remote, self.config.branch
            ))
            .map_err(ZlorbError::Git)?;

        Ok(remote_ref)
    }

    fn _should_trigger_update(
        &self,
        remote_ref: Reference,
        local_oid: Oid,
    ) -> Result<bool, ZlorbError> {
        let remote_iod: Oid = remote_ref.target().ok_or_else(|| {
            ZlorbError::Git(git2::Error::new(
                git2::ErrorCode::NotFound,
                git2::ErrorClass::None,
                "Remote ref has no target",
            ))
        })?;
        let dist_dir_path = PathBuf::from(&self.config.path).join("dist");
        let dist_dir_exists = std::fs::exists(dist_dir_path).map_err(ZlorbError::Io)?;
        let local_iod_matches_remote = local_oid == remote_iod;
        if !dist_dir_exists || !local_iod_matches_remote {
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{IndexAddOption, RepositoryInitOptions, Signature};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "zlorb-repo-processor-test-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fixture_crate(repo_path: &Path, marker: &str) {
        std::fs::create_dir_all(repo_path.join("src")).unwrap();
        std::fs::write(
            repo_path.join("Cargo.toml"),
            "[package]\nname = \"fixture-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            repo_path.join("src/main.rs"),
            format!("fn main() {{ println!(\"{}\"); }}\n", marker),
        )
        .unwrap();
    }

    fn commit_all(repo: &git2::Repository, message: &str) -> Oid {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .unwrap();
        let workdir = repo.workdir().unwrap().to_path_buf();
        let tracked: Vec<String> = index
            .iter()
            .map(|entry| String::from_utf8_lossy(&entry.path).into_owned())
            .collect();
        for path in tracked {
            if !workdir.join(&path).exists() {
                index.remove_path(Path::new(&path)).unwrap();
            }
        }
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("zlorb-test", "zlorb-test@example.com").unwrap();
        let parent = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| repo.find_commit(oid).unwrap());
        match parent {
            Some(parent) => repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])
                .unwrap(),
            None => repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &[])
                .unwrap(),
        }
    }

    fn head_oid(repo: &git2::Repository) -> Oid {
        repo.head().unwrap().target().unwrap()
    }

    fn fixture_artifact(watched: &Path) -> PathBuf {
        let name = "fixture-app";
        match std::env::var_os("CARGO_TARGET_DIR") {
            Some(dir) if Path::new(&dir).is_absolute() => {
                PathBuf::from(dir).join("debug").join(name)
            }
            _ => watched.join("target").join("debug").join(name),
        }
    }

    fn processor_for(watched: &Path) -> RepoProcessor {
        RepoProcessor::new(RepositoryConfiguration {
            name: "watched".to_string(),
            path: watched.to_string_lossy().into_owned(),
            remote: "origin".to_string(),
            branch: "master".to_string(),
            build_strategy: Some("rust".to_string()),
            build_command: String::new(),
            restart_command: None,
            out_dir: None,
        })
        .unwrap()
    }

    #[test]
    fn fast_forward_updates_working_tree_and_build_output() {
        let root = temp_dir("fast-forward");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join("old.txt"), "old file\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        drop(upstream);
        let watched = git2::Repository::clone(
            upstream_path.to_string_lossy().as_ref(),
            &watched_path,
        )
        .unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);

        processor_for(&watched_path).update_from_remote().unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        assert!(!fixture_artifact(&watched_path).exists());
        drop(watched);

        let upstream = git2::Repository::open(&upstream_path).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-two");
        std::fs::remove_file(upstream_path.join("old.txt")).unwrap();
        std::fs::write(upstream_path.join("new.txt"), "new file\n").unwrap();
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);

        processor_for(&watched_path).update_from_remote().unwrap();

        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        let source = std::fs::read_to_string(watched_path.join("src/main.rs")).unwrap();
        assert!(source.contains("fixture-version-two"));
        assert_eq!(
            std::fs::read_to_string(watched_path.join("new.txt")).unwrap(),
            "new file\n"
        );
        assert!(!watched_path.join("old.txt").exists());
        let statuses = watched.statuses(None).unwrap();
        assert!(
            statuses
                .iter()
                .all(|entry| entry.status() == git2::Status::WT_NEW),
            "no tracked file may be dirty after the fast-forward, got: {:?}",
            statuses
                .iter()
                .map(|entry| (entry.path().map(str::to_string), entry.status()))
                .collect::<Vec<_>>()
        );
        let artifact = fixture_artifact(&watched_path);
        assert!(artifact.exists());
        let output = std::process::Command::new(&artifact).output().unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("fixture-version-two")
        );
        drop(statuses);
        drop(watched);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn up_to_date_repo_is_left_untouched() {
        let root = temp_dir("up-to-date");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        let v1 = commit_all(&upstream, "v1");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        std::fs::write(watched_path.join("src/main.rs"), "local edit\n").unwrap();
        processor_for(&watched_path).update_from_remote().unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("src/main.rs")).unwrap(),
            "local edit\n"
        );
        drop(watched);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dirty_repo_refuses_fast_forward_until_cleaned() {
        let root = temp_dir("dirty-refuses");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        let v1 = commit_all(&upstream, "v1");
        write_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        std::fs::write(watched_path.join("src/main.rs"), "local edit\n").unwrap();

        let err = processor_for(&watched_path)
            .update_from_remote()
            .unwrap_err();
        assert!(format!("{:?}", err).contains("local changes to tracked files"));
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("src/main.rs")).unwrap(),
            "local edit\n"
        );
        drop(watched);

        write_fixture_crate(&watched_path, "fixture-version-one");
        processor_for(&watched_path).update_from_remote().unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-two")
        );
        drop(watched);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wip_on_a_file_the_update_does_not_touch_survives_fast_forward() {
        let root = temp_dir("wip-untouched-survives");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join("keep.txt"), "committed base\n").unwrap();
        std::fs::write(upstream_path.join("old.txt"), "old file\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        write_fixture_crate(&upstream_path, "fixture-version-two");
        std::fs::remove_file(upstream_path.join("old.txt")).unwrap();
        std::fs::write(upstream_path.join("new.txt"), "new file\n").unwrap();
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        std::fs::write(watched_path.join("keep.txt"), "standing wip\n").unwrap();

        processor_for(&watched_path).update_from_remote().unwrap();

        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-two")
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("new.txt")).unwrap(),
            "new file\n"
        );
        assert!(!watched_path.join("old.txt").exists());
        assert_eq!(
            std::fs::read_to_string(watched_path.join("keep.txt")).unwrap(),
            "standing wip\n"
        );
        let artifact = fixture_artifact(&watched_path);
        assert!(artifact.exists());
        let watched = git2::Repository::open(&watched_path).unwrap();
        let statuses = watched.statuses(None).unwrap();
        let tracked_dirty: Vec<(String, git2::Status)> = statuses
            .iter()
            .filter(|entry| entry.status() != git2::Status::WT_NEW)
            .map(|entry| {
                (
                    String::from_utf8_lossy(entry.path_bytes()).into_owned(),
                    entry.status(),
                )
            })
            .collect();
        assert_eq!(
            tracked_dirty,
            vec![("keep.txt".to_string(), git2::Status::WT_MODIFIED)]
        );
        drop(statuses);
        drop(watched);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn set_repo_config(watched: &Path, key: &str, value: &str) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(watched)
            .args(["config", key, value])
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn set_index_flag(watched: &Path, flag: &str, path: &str) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(watched)
            .args(["update-index", flag, path])
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn repo_reset_to_v1_with_upstream_v2(
        label: &str,
    ) -> (PathBuf, PathBuf, Oid, Oid) {
        let root = temp_dir(label);
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join("keep.txt"), "committed base\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        write_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        (root, watched_path, v1, v2)
    }

    #[test]
    fn skip_worktree_edit_on_a_touched_file_refuses_fast_forward() {
        let (root, watched_path, v1, _v2) =
            repo_reset_to_v1_with_upstream_v2("skip-worktree-touched");
        std::fs::write(watched_path.join("src/main.rs"), "local edit\n").unwrap();
        set_index_flag(&watched_path, "--skip-worktree", "src/main.rs");

        let processor = processor_for(&watched_path);
        assert_eq!(
            processor._tracked_local_changes().unwrap(),
            vec![PathBuf::from("src/main.rs")]
        );
        let err = processor.update_from_remote().unwrap_err();
        assert!(format!("{:?}", err).contains("local changes to tracked files"));
        drop(processor);
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("src/main.rs")).unwrap(),
            "local edit\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn assume_unchanged_edit_on_a_touched_file_refuses_fast_forward() {
        let (root, watched_path, v1, _v2) =
            repo_reset_to_v1_with_upstream_v2("assume-unchanged-touched");
        std::fs::write(watched_path.join("src/main.rs"), "local edit\n").unwrap();
        set_index_flag(&watched_path, "--assume-unchanged", "src/main.rs");

        let processor = processor_for(&watched_path);
        assert_eq!(
            processor._tracked_local_changes().unwrap(),
            vec![PathBuf::from("src/main.rs")]
        );
        let err = processor.update_from_remote().unwrap_err();
        assert!(format!("{:?}", err).contains("local changes to tracked files"));
        drop(processor);
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("src/main.rs")).unwrap(),
            "local edit\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_worktree_edit_on_an_untouched_file_survives_fast_forward() {
        let (root, watched_path, _v1, v2) =
            repo_reset_to_v1_with_upstream_v2("skip-worktree-untouched");
        std::fs::write(watched_path.join("keep.txt"), "standing wip\n").unwrap();
        set_index_flag(&watched_path, "--skip-worktree", "keep.txt");

        processor_for(&watched_path).update_from_remote().unwrap();

        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-two")
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("keep.txt")).unwrap(),
            "standing wip\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_worktree_without_an_edit_does_not_block_fast_forward() {
        let (root, watched_path, _v1, v2) =
            repo_reset_to_v1_with_upstream_v2("skip-worktree-clean");
        set_index_flag(&watched_path, "--skip-worktree", "src/main.rs");

        let processor = processor_for(&watched_path);
        assert!(processor._tracked_local_changes().unwrap().is_empty());
        processor.update_from_remote().unwrap();
        drop(processor);

        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-two")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_worktree_file_with_matching_stat_skips_the_hash_check() {
        let (root, watched_path, _v1, _v2) =
            repo_reset_to_v1_with_upstream_v2("skip-worktree-stat-shortcut");
        std::fs::write(
            watched_path.join("src/main.rs"),
            "fn main() { println!(\"local-edit-versionx\"); }\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::MetadataExt;
            let watched = git2::Repository::open(&watched_path).unwrap();
            let mut index = watched.index().unwrap();
            let mut entry = index.get_path(Path::new("src/main.rs"), 0).unwrap();
            let metadata = std::fs::metadata(watched_path.join("src/main.rs")).unwrap();
            entry.mtime =
                git2::IndexTime::new(metadata.mtime() as i32, metadata.mtime_nsec() as u32);
            entry.ctime =
                git2::IndexTime::new(metadata.ctime() as i32, metadata.ctime_nsec() as u32);
            entry.ino = metadata.ino() as u32;
            entry.file_size = metadata.len() as u32;
            entry.flags_extended |= 0x4000;
            index.add(&entry).unwrap();
            index.write().unwrap();
        }

        let processor = processor_for(&watched_path);
        assert!(processor._tracked_local_changes().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_worktree_file_with_stale_stat_falls_back_to_the_hash_check() {
        let (root, watched_path, _v1, _v2) =
            repo_reset_to_v1_with_upstream_v2("skip-worktree-stat-stale");
        set_index_flag(&watched_path, "--skip-worktree", "src/main.rs");
        std::fs::write(
            watched_path.join("src/main.rs"),
            "fn main() { println!(\"local-edit-versionx\"); }\n",
        )
        .unwrap();

        let processor = processor_for(&watched_path);
        assert_eq!(
            processor._tracked_local_changes().unwrap(),
            vec![PathBuf::from("src/main.rs")]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_worktree_file_with_only_mtime_restored_still_falls_back_to_the_hash_check() {
        let (root, watched_path, _v1, _v2) =
            repo_reset_to_v1_with_upstream_v2("skip-worktree-stat-mtime-restored");
        set_index_flag(&watched_path, "--skip-worktree", "src/main.rs");
        let recorded_mtime = std::fs::metadata(watched_path.join("src/main.rs"))
            .unwrap()
            .modified()
            .unwrap();
        std::fs::write(
            watched_path.join("src/main.rs"),
            "fn main() { println!(\"local-edit-versionx\"); }\n",
        )
        .unwrap();
        let file = std::fs::File::options()
            .write(true)
            .open(watched_path.join("src/main.rs"))
            .unwrap();
        file.set_modified(recorded_mtime).unwrap();
        drop(file);

        let processor = processor_for(&watched_path);
        assert_eq!(
            processor._tracked_local_changes().unwrap(),
            vec![PathBuf::from("src/main.rs")]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_worktree_retargeted_symlink_on_a_touched_path_refuses_fast_forward() {
        let root = temp_dir("skip-worktree-symlink");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::os::unix::fs::symlink("src/main.rs", upstream_path.join("link.txt")).unwrap();
        let v1 = commit_all(&upstream, "v1");
        std::fs::remove_file(upstream_path.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("Cargo.toml", upstream_path.join("link.txt")).unwrap();
        commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        std::fs::remove_file(watched_path.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("local-target", watched_path.join("link.txt")).unwrap();
        set_index_flag(&watched_path, "--skip-worktree", "link.txt");

        let processor = processor_for(&watched_path);
        assert_eq!(
            processor._tracked_local_changes().unwrap(),
            vec![PathBuf::from("link.txt")]
        );
        let err = processor.update_from_remote().unwrap_err();
        assert!(format!("{:?}", err).contains("local changes to tracked files"));
        drop(processor);
        assert_eq!(
            std::fs::read_link(watched_path.join("link.txt")).unwrap(),
            PathBuf::from("local-target")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_worktree_file_matching_its_blob_after_clean_filters_is_not_a_change() {
        let root = temp_dir("skip-worktree-crlf");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();
        std::fs::write(upstream_path.join("data.txt"), "line one\nline two\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        write_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        std::fs::write(watched_path.join("data.txt"), "line one\r\nline two\r\n").unwrap();
        set_index_flag(&watched_path, "--skip-worktree", "data.txt");

        let processor = processor_for(&watched_path);
        assert!(processor._tracked_local_changes().unwrap().is_empty());
        processor.update_from_remote().unwrap();
        drop(processor);

        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert_eq!(
            std::fs::read(watched_path.join("data.txt")).unwrap(),
            b"line one\r\nline two\r\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_worktree_file_matching_its_blob_after_an_external_clean_filter_is_not_a_change() {
        let root = temp_dir("skip-worktree-external-filter");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join(".gitattributes"), "*.txt filter=strip\n").unwrap();
        std::fs::write(upstream_path.join("data.txt"), "hello\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        write_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        set_repo_config(&watched_path, "filter.strip.clean", "sed 's/^X//'");
        std::fs::write(watched_path.join("data.txt"), "Xhello\n").unwrap();
        set_index_flag(&watched_path, "--skip-worktree", "data.txt");

        let processor = processor_for(&watched_path);
        assert!(processor._tracked_local_changes().unwrap().is_empty());
        processor.update_from_remote().unwrap();
        drop(processor);

        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert_eq!(
            std::fs::read(watched_path.join("data.txt")).unwrap(),
            b"Xhello\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_worktree_file_changed_after_an_external_clean_filter_still_refuses_fast_forward() {
        let root = temp_dir("skip-worktree-external-filter-changed");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join(".gitattributes"), "*.txt filter=strip\n").unwrap();
        std::fs::write(upstream_path.join("data.txt"), "hello\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        std::fs::write(upstream_path.join("data.txt"), "version two\n").unwrap();
        commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        set_repo_config(&watched_path, "filter.strip.clean", "sed 's/^X//'");
        std::fs::write(watched_path.join("data.txt"), "Xgoodbye\n").unwrap();
        set_index_flag(&watched_path, "--skip-worktree", "data.txt");

        let processor = processor_for(&watched_path);
        assert_eq!(
            processor._tracked_local_changes().unwrap(),
            vec![PathBuf::from("data.txt")]
        );
        let err = processor.update_from_remote().unwrap_err();
        assert!(format!("{:?}", err).contains("local changes to tracked files"));
        drop(processor);
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        assert_eq!(
            std::fs::read(watched_path.join("data.txt")).unwrap(),
            b"Xgoodbye\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn pipe_pair() -> (std::fs::File, std::fs::File) {
        use std::os::unix::io::FromRawFd;
        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let write = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        (read, write)
    }

    fn closed_pipe_pair() -> std::fs::File {
        let (read, write) = pipe_pair();
        drop(write);
        read
    }

    fn process_is_running(pid: i32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some(close) = stat.rfind(')') else {
            return false;
        };
        stat[close + 1..]
            .split_whitespace()
            .next()
            .map(|state| state != "Z")
            .unwrap_or(false)
    }

    fn spawn_sleep_group() -> (std::process::Child, libc::pid_t) {
        use std::os::unix::process::CommandExt;
        let child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as libc::pid_t;
        (child, pgid)
    }

    #[test]
    fn two_pipe_drains_sharing_one_deadline_finish_within_the_shared_budget() {
        let (mut child, pgid) = spawn_sleep_group();
        let (out_read, _out_write) = pipe_pair();
        let (err_read, _err_write) = pipe_pair();
        let mut drains = PipeDrains::new(out_read, err_read);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
        let started = std::time::Instant::now();
        assert!(!drains.finish(deadline, pgid));
        assert!(started.elapsed() < std::time::Duration::from_millis(1500));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn pipe_drain_captures_buffered_output_when_the_kill_closes_the_pipe() {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("printf 'late\\n'; sleep 30")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as libc::pid_t;
        let mut drains = PipeDrains::new(
            child.stdout.take().unwrap(),
            child.stderr.take().unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
        assert!(drains.finish(deadline, pgid));
        assert_eq!(drains.stdout.buf, b"late\n".to_vec());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn pipe_drain_before_the_shared_deadline_returns_the_buffered_output() {
        use std::io::Write;
        let (mut child, pgid) = spawn_sleep_group();
        let (out_read, mut out_write) = pipe_pair();
        out_write.write_all(b"hello\n").unwrap();
        drop(out_write);
        let mut drains = PipeDrains::new(out_read, closed_pipe_pair());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        assert!(drains.finish(deadline, pgid));
        assert_eq!(drains.stdout.buf, b"hello\n".to_vec());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn pipe_drain_deadline_closes_the_read_end_held_open_by_an_escapee() {
        let (mut child, pgid) = spawn_sleep_group();
        let (out_read, _out_write) = pipe_pair();
        let mut drains = PipeDrains::new(out_read, closed_pipe_pair());
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        assert!(!drains.finish(deadline, pgid));
        let fd = drains.stdout.fd;
        let target_before = std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap();
        assert!(target_before.to_string_lossy().starts_with("pipe:"));
        drop(drains);
        let target_after = std::fs::read_link(format!("/proc/self/fd/{fd}"));
        assert!(
            target_after.map(|target| target != target_before).unwrap_or(true),
            "read end fd {fd} still points at the drained pipe after the drains were dropped"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn pipe_drain_deadline_kills_a_setsid_process_holding_the_pipe() {
        use std::os::unix::process::ExitStatusExt;
        let (out_read, out_write) = pipe_pair();
        let mut escapee = Command::new("setsid")
            .arg("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(out_write))
            .spawn()
            .unwrap();
        let (mut child, pgid) = spawn_sleep_group();
        let mut drains = PipeDrains::new(out_read, closed_pipe_pair());
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
        assert!(drains.finish(deadline, pgid));
        let mut status = None;
        for _ in 0..60 {
            if let Ok(Some(found)) = escapee.try_wait() {
                status = Some(found);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let status = match status {
            Some(status) => status,
            None => {
                let _ = escapee.kill();
                let _ = escapee.wait();
                panic!("setsid process holding the pipe was not killed at the drain deadline");
            }
        };
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn kill_pipe_holders_leaves_read_end_and_other_pipe_holders_alive() {
        use std::os::unix::io::AsRawFd;
        use std::os::unix::process::ExitStatusExt;
        let (target_read, target_write) = pipe_pair();
        let target = pipe_target(target_read.as_raw_fd()).unwrap();
        let mut victim = Command::new("setsid")
            .arg("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(target_write))
            .spawn()
            .unwrap();
        let mut read_holder = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::from(target_read))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (_other_read, other_write) = pipe_pair();
        let mut other_pipe_holder = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(other_write))
            .spawn()
            .unwrap();
        kill_pipe_holders(&[target]);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            matches!(read_holder.try_wait(), Ok(None)),
            "a process holding only the read end of the pipe was killed"
        );
        assert!(
            matches!(other_pipe_holder.try_wait(), Ok(None)),
            "a process holding the write end of a different pipe was killed"
        );
        let mut victim_status = None;
        for _ in 0..60 {
            if let Ok(Some(found)) = victim.try_wait() {
                victim_status = Some(found);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        match victim_status {
            Some(found) => assert_eq!(found.signal(), Some(libc::SIGKILL)),
            None => {
                let _ = victim.kill();
                let _ = victim.wait();
                panic!("write-end holder of the target pipe survived the same sweep");
            }
        }
        let _ = read_holder.kill();
        let _ = read_holder.wait();
        let _ = other_pipe_holder.kill();
        let _ = other_pipe_holder.wait();
    }

    #[test]
    fn pipe_drain_second_sweep_kills_a_holder_that_grabs_the_pipe_after_the_first_sweep() {
        use std::os::unix::io::AsRawFd;
        use std::os::unix::process::ExitStatusExt;
        let root = temp_dir("pipe-drain-second-sweep");
        std::fs::create_dir_all(&root).unwrap();
        let body = format!(
            "#!/bin/sh\n( sleep 0.68; exec sleep 30 2>/proc/$1/fd/$2 ) 2>/dev/null &\necho $! > {}/late.pid\nexec sleep 30\n",
            root.display()
        );
        let script = write_filter_script(&root, "late-holder.sh", &body);
        let (out_read, out_write) = pipe_pair();
        let target = pipe_target(out_read.as_raw_fd()).unwrap();
        let donor = out_write.try_clone().unwrap();
        let donor_fd = donor.as_raw_fd();
        let donor_dropper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(900));
            drop(donor);
        });
        let mut escapee = Command::new("setsid")
            .arg(&script)
            .arg(std::process::id().to_string())
            .arg(donor_fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(out_write))
            .spawn()
            .unwrap();
        let pid_path = root.join("late.pid");
        let observer = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut late_pid = None;
            let mut holding_since = None;
            while started.elapsed() < std::time::Duration::from_millis(2500) {
                if late_pid.is_none() {
                    if let Ok(text) = std::fs::read_to_string(&pid_path) {
                        late_pid = text.trim().parse::<libc::pid_t>().ok();
                    }
                }
                if let Some(pid) = late_pid {
                    if holding_since.is_none()
                        && !process_pipe_write_targets(pid, std::slice::from_ref(&target))
                            .is_empty()
                    {
                        holding_since = Some(started.elapsed());
                    }
                    if holding_since.is_some() && !process_is_running(pid as i32) {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            (late_pid, holding_since)
        });
        let (mut child, pgid) = spawn_sleep_group();
        let mut drains = PipeDrains::new(out_read, closed_pipe_pair());
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
        assert!(drains.finish(deadline, pgid));
        let mut status = None;
        for _ in 0..60 {
            if let Ok(Some(found)) = escapee.try_wait() {
                status = Some(found);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        match status {
            Some(found) => assert_eq!(found.signal(), Some(libc::SIGKILL)),
            None => {
                let _ = escapee.kill();
                let _ = escapee.wait();
                panic!("setsid process holding the pipe was not killed at the drain deadline");
            }
        }
        let (late_pid, holding_since) = observer.join().unwrap();
        let late_pid = late_pid.expect("the late holder script never recorded its pid");
        let holding_since =
            holding_since.expect("the late holder was never observed holding the target pipe");
        assert!(
            holding_since >= std::time::Duration::from_millis(500),
            "the late holder grabbed the pipe too early: {holding_since:?}"
        );
        let mut late_dead = false;
        for _ in 0..60 {
            if !process_is_running(late_pid as i32) {
                late_dead = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(late_dead, "late holder {late_pid} is still running after finish");
        let _ = donor_dropper.join();
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hanging_external_clean_filter_times_out_and_is_treated_as_a_change() {
        let root = temp_dir("skip-worktree-external-filter-hang");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join(".gitattributes"), "*.txt filter=hang\n").unwrap();
        std::fs::write(upstream_path.join("data.txt"), "hello\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        set_repo_config(&watched_path, "filter.hang.clean", "sleep 5");
        std::fs::write(watched_path.join("data.txt"), "Xhello\n").unwrap();
        set_index_flag(&watched_path, "--skip-worktree", "data.txt");

        let processor = processor_for(&watched_path);
        let entry_id = processor
            .repo
            .index()
            .unwrap()
            .get_path(Path::new("data.txt"), 0)
            .unwrap()
            .id;
        let started = std::time::Instant::now();
        let matched = processor._matches_after_external_clean_filter_with_timeout(
            &watched_path,
            Path::new("data.txt"),
            &watched_path.join("data.txt"),
            entry_id,
            std::time::Duration::from_millis(200),
        );
        assert!(!matched);
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn write_filter_script(root: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = root.join(name);
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn timed_out_external_clean_filter_takes_its_process_group_down_with_it() {
        let root = temp_dir("skip-worktree-external-filter-group-kill");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join(".gitattributes"), "*.txt filter=hang\n").unwrap();
        std::fs::write(upstream_path.join("data.txt"), "hello\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        let script = write_filter_script(
            &root,
            "hang-filter.sh",
            "#!/bin/sh\nwhile :; do echo tick >> \"$(dirname \"$0\")/ticks\"; sleep 0.1; done &\nwait\n",
        );
        set_repo_config(
            &watched_path,
            "filter.hang.clean",
            script.to_string_lossy().as_ref(),
        );
        std::fs::write(watched_path.join("data.txt"), "Xhello\n").unwrap();
        set_index_flag(&watched_path, "--skip-worktree", "data.txt");

        let processor = processor_for(&watched_path);
        let entry_id = processor
            .repo
            .index()
            .unwrap()
            .get_path(Path::new("data.txt"), 0)
            .unwrap()
            .id;
        let started = std::time::Instant::now();
        let matched = processor._matches_after_external_clean_filter_with_timeout(
            &watched_path,
            Path::new("data.txt"),
            &watched_path.join("data.txt"),
            entry_id,
            std::time::Duration::from_millis(200),
        );
        assert!(!matched);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let ticks = root.join("ticks");
        assert!(ticks.exists());
        let size_before = std::fs::metadata(&ticks).unwrap().len();
        std::thread::sleep(std::time::Duration::from_millis(700));
        assert_eq!(std::fs::metadata(&ticks).unwrap().len(), size_before);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn external_clean_filter_descendant_holding_stderr_does_not_block_the_recheck() {
        let root = temp_dir("skip-worktree-external-filter-pipe-holder");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join(".gitattributes"), "*.txt filter=strip\n").unwrap();
        std::fs::write(upstream_path.join("data.txt"), "hello\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        let script = write_filter_script(
            &root,
            "strip-filter.sh",
            "#!/bin/sh\nsed 's/^X//'\nsleep 30 >/dev/null &\n",
        );
        set_repo_config(
            &watched_path,
            "filter.strip.clean",
            script.to_string_lossy().as_ref(),
        );
        std::fs::write(watched_path.join("data.txt"), "Xhello\n").unwrap();
        set_index_flag(&watched_path, "--skip-worktree", "data.txt");

        let processor = processor_for(&watched_path);
        let entry_id = processor
            .repo
            .index()
            .unwrap()
            .get_path(Path::new("data.txt"), 0)
            .unwrap()
            .id;
        let started = std::time::Instant::now();
        let matched = processor._matches_after_external_clean_filter_with_timeout(
            &watched_path,
            Path::new("data.txt"),
            &watched_path.join("data.txt"),
            entry_id,
            std::time::Duration::from_secs(30),
        );
        assert!(matched);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn external_clean_filter_flooding_stderr_still_completes_the_recheck() {
        let root = temp_dir("skip-worktree-external-filter-stderr-flood");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join(".gitattributes"), "*.txt filter=strip\n").unwrap();
        std::fs::write(upstream_path.join("data.txt"), "hello\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        let script = write_filter_script(
            &root,
            "strip-filter-flood.sh",
            "#!/bin/sh\ntr '\\0' 'e' < /dev/zero | head -c 200000 >&2\nsed 's/^X//'\n",
        );
        set_repo_config(
            &watched_path,
            "filter.strip.clean",
            script.to_string_lossy().as_ref(),
        );
        std::fs::write(watched_path.join("data.txt"), "Xhello\n").unwrap();
        set_index_flag(&watched_path, "--skip-worktree", "data.txt");

        let processor = processor_for(&watched_path);
        let entry_id = processor
            .repo
            .index()
            .unwrap()
            .get_path(Path::new("data.txt"), 0)
            .unwrap()
            .id;
        let started = std::time::Instant::now();
        let matched = processor._matches_after_external_clean_filter_with_timeout(
            &watched_path,
            Path::new("data.txt"),
            &watched_path.join("data.txt"),
            entry_id,
            std::time::Duration::from_secs(15),
        );
        assert!(matched);
        assert!(started.elapsed() < std::time::Duration::from_secs(12));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn external_clean_filter_setsid_descendant_holding_stderr_still_completes_the_recheck() {
        let root = temp_dir("skip-worktree-external-filter-setsid-holder");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join(".gitattributes"), "*.txt filter=strip\n").unwrap();
        std::fs::write(upstream_path.join("data.txt"), "hello\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        let script = write_filter_script(
            &root,
            "strip-filter-setsid.sh",
            "#!/bin/sh\nsed 's/^X//'\nsetsid sleep 30 >/dev/null &\n",
        );
        set_repo_config(
            &watched_path,
            "filter.strip.clean",
            script.to_string_lossy().as_ref(),
        );
        std::fs::write(watched_path.join("data.txt"), "Xhello\n").unwrap();
        set_index_flag(&watched_path, "--skip-worktree", "data.txt");

        let processor = processor_for(&watched_path);
        let entry_id = processor
            .repo
            .index()
            .unwrap()
            .get_path(Path::new("data.txt"), 0)
            .unwrap()
            .id;
        let started = std::time::Instant::now();
        let matched = processor._matches_after_external_clean_filter_with_timeout(
            &watched_path,
            Path::new("data.txt"),
            &watched_path.join("data.txt"),
            entry_id,
            std::time::Duration::from_secs(30),
        );
        assert!(matched);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn external_clean_filter_setsid_descendant_holding_a_pipe_is_killed() {
        let root = temp_dir("skip-worktree-external-filter-setsid-kill");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join(".gitattributes"), "*.txt filter=strip\n").unwrap();
        std::fs::write(upstream_path.join("data.txt"), "hello\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        let escapee = write_filter_script(
            &root,
            "escapee.sh",
            &format!(
                "#!/bin/sh\necho $$ > \"{}\"\nexec sleep 30\n",
                root.join("escapee.pid").display()
            ),
        );
        let script = write_filter_script(
            &root,
            "strip-filter-setsid-kill.sh",
            &format!(
                "#!/bin/sh\nsed 's/^X//'\nsetsid \"{}\" >/dev/null &\n",
                escapee.display()
            ),
        );
        set_repo_config(
            &watched_path,
            "filter.strip.clean",
            script.to_string_lossy().as_ref(),
        );
        std::fs::write(watched_path.join("data.txt"), "Xhello\n").unwrap();
        set_index_flag(&watched_path, "--skip-worktree", "data.txt");

        let processor = processor_for(&watched_path);
        let entry_id = processor
            .repo
            .index()
            .unwrap()
            .get_path(Path::new("data.txt"), 0)
            .unwrap()
            .id;
        let started = std::time::Instant::now();
        let matched = processor._matches_after_external_clean_filter_with_timeout(
            &watched_path,
            Path::new("data.txt"),
            &watched_path.join("data.txt"),
            entry_id,
            std::time::Duration::from_secs(30),
        );
        assert!(matched);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        let pid: i32 = std::fs::read_to_string(root.join("escapee.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while process_is_running(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let running = process_is_running(pid);
        if running {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        assert!(!running, "setsid descendant {pid} survived the recheck");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn assume_unchanged_deleted_file_refuses_fast_forward() {
        let (root, watched_path, v1, _v2) =
            repo_reset_to_v1_with_upstream_v2("assume-unchanged-deleted");
        set_index_flag(&watched_path, "--assume-unchanged", "src/main.rs");
        std::fs::remove_file(watched_path.join("src/main.rs")).unwrap();

        let processor = processor_for(&watched_path);
        assert_eq!(
            processor._tracked_local_changes().unwrap(),
            vec![PathBuf::from("src/main.rs")]
        );
        let err = processor.update_from_remote().unwrap_err();
        assert!(format!("{:?}", err).contains("local changes to tracked files"));
        drop(processor);
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        assert!(!watched_path.join("src/main.rs").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_worktree_deleted_file_survives_fast_forward() {
        let (root, watched_path, _v1, v2) =
            repo_reset_to_v1_with_upstream_v2("skip-worktree-deleted");
        set_index_flag(&watched_path, "--skip-worktree", "keep.txt");
        std::fs::remove_file(watched_path.join("keep.txt")).unwrap();

        let processor = processor_for(&watched_path);
        assert_eq!(
            processor._tracked_local_changes().unwrap(),
            vec![PathBuf::from("keep.txt")]
        );
        processor.update_from_remote().unwrap();
        drop(processor);

        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert!(!watched_path.join("keep.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_worktree_mode_change_on_a_touched_file_refuses_fast_forward() {
        let (root, watched_path, v1, _v2) =
            repo_reset_to_v1_with_upstream_v2("skip-worktree-mode");
        set_index_flag(&watched_path, "--skip-worktree", "src/main.rs");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions =
                std::fs::metadata(watched_path.join("src/main.rs")).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(watched_path.join("src/main.rs"), permissions).unwrap();
        }

        let processor = processor_for(&watched_path);
        assert_eq!(
            processor._tracked_local_changes().unwrap(),
            vec![PathBuf::from("src/main.rs")]
        );
        let err = processor.update_from_remote().unwrap_err();
        assert!(format!("{:?}", err).contains("local changes to tracked files"));
        drop(processor);
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn assume_unchanged_file_replaced_by_a_directory_refuses_fast_forward() {
        let (root, watched_path, v1, _v2) =
            repo_reset_to_v1_with_upstream_v2("assume-unchanged-dir");
        set_index_flag(&watched_path, "--assume-unchanged", "src/main.rs");
        std::fs::remove_file(watched_path.join("src/main.rs")).unwrap();
        std::fs::create_dir(watched_path.join("src/main.rs")).unwrap();
        std::fs::write(watched_path.join("src/main.rs/precious.txt"), "precious\n").unwrap();

        let processor = processor_for(&watched_path);
        assert_eq!(
            processor._tracked_local_changes().unwrap(),
            vec![PathBuf::from("src/main.rs")]
        );
        let err = processor.update_from_remote().unwrap_err();
        assert!(format!("{:?}", err).contains("local changes to tracked files"));
        drop(processor);
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("src/main.rs/precious.txt")).unwrap(),
            "precious\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skip_worktree_file_replaced_by_a_directory_refuses_fast_forward() {
        let (root, watched_path, v1, _v2) =
            repo_reset_to_v1_with_upstream_v2("skip-worktree-dir");
        set_index_flag(&watched_path, "--skip-worktree", "src/main.rs");
        std::fs::remove_file(watched_path.join("src/main.rs")).unwrap();
        std::fs::create_dir(watched_path.join("src/main.rs")).unwrap();
        std::fs::write(watched_path.join("src/main.rs/precious.txt"), "precious\n").unwrap();

        let processor = processor_for(&watched_path);
        assert_eq!(
            processor._tracked_local_changes().unwrap(),
            vec![PathBuf::from("src/main.rs")]
        );
        let err = processor.update_from_remote().unwrap_err();
        assert!(format!("{:?}", err).contains("local changes to tracked files"));
        drop(processor);
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("src/main.rs/precious.txt")).unwrap(),
            "precious\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_wip_on_a_file_the_update_does_not_touch() {
        let root = temp_dir("rollback-wip-untouched");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::write(watched_path.join("keep.txt"), "committed base\n").unwrap();
        std::fs::write(watched_path.join("only-v1.txt"), "only in v1\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        std::fs::remove_file(watched_path.join("only-v1.txt")).unwrap();
        std::fs::write(watched_path.join("new.txt"), "new in v2\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        std::fs::write(watched_path.join("keep.txt"), "standing wip\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-one")
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("keep.txt")).unwrap(),
            "standing wip\n"
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("only-v1.txt")).unwrap(),
            "only in v1\n"
        );
        assert!(!watched_path.join("new.txt").exists());
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_fast_forward_rechecks_guard_and_preserves_edit_made_after_first_check() {
        let (root, watched_path, v1, v2) =
            repo_reset_to_v1_with_upstream_v2("apply-recheck");
        let processor = processor_for(&watched_path);
        let changed = processor._changed_paths(v1, v2).unwrap();
        {
            let first_ref = processor
                .repo
                .find_reference("refs/remotes/origin/master")
                .unwrap();
            let first_commit = processor
                .repo
                .reference_to_annotated_commit(&first_ref)
                .unwrap();
            assert!(
                processor
                    ._refuse_if_update_would_overwrite(&first_commit, Some(&changed))
                    .is_ok()
            );
        }
        std::fs::write(watched_path.join("src/main.rs"), "race edit\n").unwrap();
        {
            let second_ref = processor
                .repo
                .find_reference("refs/remotes/origin/master")
                .unwrap();
            let second_commit = processor
                .repo
                .reference_to_annotated_commit(&second_ref)
                .unwrap();
            match processor._apply_fast_forward(second_commit, v1, Some(&changed)) {
                Ok(_) => {
                    panic!("apply must refuse an edit made after the first guard check")
                }
                Err(e) => assert!(
                    format!("{:?}", e).contains("local changes to tracked files"),
                    "unexpected error: {:?}",
                    e
                ),
            }
        }
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("src/main.rs")).unwrap(),
            "race edit\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn two_file_repo(label: &str) -> (PathBuf, PathBuf, Oid, Oid) {
        let root = temp_dir(label);
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        std::fs::write(watched_path.join("a.txt"), "one-a\n").unwrap();
        std::fs::write(watched_path.join("b.txt"), "one-b\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::write(watched_path.join("a.txt"), "two-a\n").unwrap();
        std::fs::write(watched_path.join("b.txt"), "two-b\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        let v1_commit = watched.find_commit(v1).unwrap();
        watched
            .reset(v1_commit.as_object(), git2::ResetType::Mixed, None)
            .unwrap();
        drop(v1_commit);
        drop(watched);
        std::fs::write(watched_path.join("a.txt"), "one-a\n").unwrap();
        std::fs::write(watched_path.join("b.txt"), "one-b\n").unwrap();
        (root, watched_path, v1, v2)
    }

    fn move_ref_to_v2(processor: &RepoProcessor, v2: Oid) {
        processor
            .repo
            .reference("refs/remotes/origin/master", v2, true, "test")
            .unwrap();
        let fetch_ref = processor
            .repo
            .find_reference("refs/remotes/origin/master")
            .unwrap();
        let fetch_commit = processor
            .repo
            .reference_to_annotated_commit(&fetch_ref)
            .unwrap();
        processor._setup_repo_for_checkout(fetch_commit).unwrap();
    }

    #[test]
    fn checkout_step_refuses_edit_made_after_the_ref_moved() {
        let (root, watched_path, v1, v2) = two_file_repo("checkout-revalidate-refuses");
        let processor = processor_for(&watched_path);
        move_ref_to_v2(&processor, v2);
        assert_eq!(head_oid(&processor.repo), v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        std::fs::write(watched_path.join("b.txt"), "race edit\n").unwrap();
        match processor._checkout_and_get_ref(Some(v1), v2, Some(&changed)) {
            Ok(_) => panic!("checkout must refuse an edit made after the ref moved"),
            Err(e) => assert!(
                format!("{:?}", e).contains("b.txt"),
                "unexpected error: {:?}",
                e
            ),
        }
        assert_eq!(
            std::fs::read_to_string(watched_path.join("a.txt")).unwrap(),
            "one-a\n"
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("b.txt")).unwrap(),
            "race edit\n"
        );
        assert!(!processor._rollback_failed_apply(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("a.txt")).unwrap(),
            "one-a\n"
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("b.txt")).unwrap(),
            "race edit\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_deletion_made_after_the_ref_moved() {
        let (root, watched_path, v1, v2) = two_file_repo("checkout-revalidate-deletion");
        let processor = processor_for(&watched_path);
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        std::fs::remove_file(watched_path.join("b.txt")).unwrap();
        match processor._checkout_and_get_ref(Some(v1), v2, Some(&changed)) {
            Ok(_) => panic!("checkout must refuse a deletion made after the ref moved"),
            Err(e) => assert!(
                format!("{:?}", e).contains("b.txt"),
                "unexpected error: {:?}",
                e
            ),
        }
        assert_eq!(
            std::fs::read_to_string(watched_path.join("a.txt")).unwrap(),
            "one-a\n"
        );
        assert!(!watched_path.join("b.txt").exists());
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_accepts_disk_content_matching_the_fetched_commit() {
        let (root, watched_path, v1, v2) =
            two_file_repo("checkout-revalidate-matches-fetched");
        let processor = processor_for(&watched_path);
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        std::fs::write(watched_path.join("b.txt"), "two-b\n").unwrap();
        let remote_ref = processor
            ._checkout_and_get_ref(Some(v1), v2, Some(&changed))
            .unwrap();
        assert_eq!(remote_ref.target(), Some(v2));
        drop(remote_ref);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("a.txt")).unwrap(),
            "two-a\n"
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("b.txt")).unwrap(),
            "two-b\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_dir_to_file_when_the_directory_holds_an_untracked_file() {
        let root = temp_dir("checkout-revalidate-dir-junk");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        std::fs::create_dir_all(watched_path.join("thing")).unwrap();
        std::fs::write(watched_path.join("thing/inner.txt"), "inner\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::remove_dir_all(watched_path.join("thing")).unwrap();
        std::fs::write(watched_path.join("thing"), "now a file\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        let v1_commit = watched.find_commit(v1).unwrap();
        watched
            .reset(v1_commit.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(v1_commit);
        drop(watched);
        std::fs::write(watched_path.join("thing/junk.txt"), "junk\n").unwrap();

        let processor = processor_for(&watched_path);
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        match processor._checkout_and_get_ref(Some(v1), v2, Some(&changed)) {
            Ok(_) => panic!("checkout must refuse when a changed directory holds an untracked file"),
            Err(e) => assert!(
                format!("{:?}", e).contains("thing"),
                "unexpected error: {:?}",
                e
            ),
        }
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/junk.txt")).unwrap(),
            "junk\n"
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/inner.txt")).unwrap(),
            "inner\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn dir_to_file_repo_with_gitignore(
        label: &str,
        gitignore: &str,
    ) -> (PathBuf, PathBuf, Oid, Oid) {
        let root = temp_dir(label);
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        std::fs::write(watched_path.join(".gitignore"), gitignore).unwrap();
        std::fs::create_dir_all(watched_path.join("thing")).unwrap();
        std::fs::write(watched_path.join("thing/inner.txt"), "inner\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::remove_dir_all(watched_path.join("thing")).unwrap();
        std::fs::write(watched_path.join("thing"), "now a file\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        let v1_commit = watched.find_commit(v1).unwrap();
        watched
            .reset(v1_commit.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(v1_commit);
        drop(watched);
        (root, watched_path, v1, v2)
    }

    #[test]
    fn checkout_step_accepts_dir_to_file_when_the_directory_holds_an_ignored_file() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-ignored-file", "*.log\n");
        std::fs::write(watched_path.join("thing/debug.log"), "ignored\n").unwrap();

        let processor = processor_for(&watched_path);
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        let remote_ref = processor
            ._checkout_and_get_ref(Some(v1), v2, Some(&changed))
            .unwrap();
        assert_eq!(remote_ref.target(), Some(v2));
        drop(remote_ref);
        drop(processor);
        assert!(watched_path.join("thing").is_file());
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing")).unwrap(),
            "now a file\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_accepts_dir_to_file_when_an_extra_directory_holds_only_ignored_files() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-extra-ignored-only", "*.log\n");
        std::fs::create_dir_all(watched_path.join("thing/cache/nested")).unwrap();
        std::fs::write(watched_path.join("thing/cache/debug.log"), "ignored\n").unwrap();
        std::fs::write(
            watched_path.join("thing/cache/nested/trace.log"),
            "ignored\n",
        )
        .unwrap();

        let processor = processor_for(&watched_path);
        assert!(processor._checkout_dir_is_safe(v1, v2, Path::new("thing")));
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        let remote_ref = processor
            ._checkout_and_get_ref(Some(v1), v2, Some(&changed))
            .unwrap();
        assert_eq!(remote_ref.target(), Some(v2));
        drop(remote_ref);
        drop(processor);
        assert!(watched_path.join("thing").is_file());
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing")).unwrap(),
            "now a file\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_accepts_dir_to_file_when_an_extra_directory_is_itself_ignored() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-extra-ignored-dir", "cache/\n");
        std::fs::create_dir_all(watched_path.join("thing/cache")).unwrap();
        std::fs::write(watched_path.join("thing/cache/data.bin"), "ignored\n").unwrap();

        let processor = processor_for(&watched_path);
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        let remote_ref = processor
            ._checkout_and_get_ref(Some(v1), v2, Some(&changed))
            .unwrap();
        assert_eq!(remote_ref.target(), Some(v2));
        drop(remote_ref);
        drop(processor);
        assert!(watched_path.join("thing").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_dir_to_file_when_an_extra_directory_mixes_ignored_and_plain_files() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-extra-mixed", "*.log\n");
        std::fs::create_dir_all(watched_path.join("thing/cache")).unwrap();
        std::fs::write(watched_path.join("thing/cache/debug.log"), "ignored\n").unwrap();
        std::fs::write(watched_path.join("thing/cache/keep.txt"), "plain\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._checkout_dir_is_safe(v1, v2, Path::new("thing")));
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        match processor._checkout_and_get_ref(Some(v1), v2, Some(&changed)) {
            Ok(_) => panic!(
                "checkout must refuse when an extra directory mixes ignored and plain files"
            ),
            Err(e) => assert!(
                format!("{:?}", e).contains("thing"),
                "unexpected error: {:?}",
                e
            ),
        }
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/cache/keep.txt")).unwrap(),
            "plain\n"
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/cache/debug.log")).unwrap(),
            "ignored\n"
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/inner.txt")).unwrap(),
            "inner\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_dir_to_file_when_an_extra_directory_holds_a_staged_ignored_file() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-extra-staged-ignored", "*.log\n");
        std::fs::create_dir_all(watched_path.join("thing/cache")).unwrap();
        std::fs::write(watched_path.join("thing/cache/debug.log"), "staged\n").unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let mut index = watched.index().unwrap();
        index
            .add_path(Path::new("thing/cache/debug.log"))
            .unwrap();
        index.write().unwrap();
        drop(index);
        drop(watched);

        let processor = processor_for(&watched_path);
        assert!(!processor._checkout_dir_is_safe(v1, v2, Path::new("thing")));
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        match processor._checkout_and_get_ref(Some(v1), v2, Some(&changed)) {
            Ok(_) => panic!(
                "checkout must refuse when an extra directory holds a staged ignored file"
            ),
            Err(e) => assert!(
                format!("{:?}", e).contains("thing"),
                "unexpected error: {:?}",
                e
            ),
        }
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/cache/debug.log")).unwrap(),
            "staged\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn add_index_entry_at_stage(
        repo: &git2::Repository,
        path: &Path,
        id: Oid,
        mode: u32,
        stage: i32,
    ) {
        let mut index = repo.index().unwrap();
        let path_bytes = {
            use std::os::unix::ffi::OsStrExt;
            path.as_os_str().as_bytes().to_vec()
        };
        let flags =
            ((stage as u16) << 12) | (path_bytes.len().min(0xfff) as u16);
        index
            .add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode,
                uid: 0,
                gid: 0,
                file_size: 0,
                id,
                flags,
                flags_extended: 0,
                path: path_bytes,
            })
            .unwrap();
        index.write().unwrap();
    }

    #[test]
    fn checkout_step_refuses_dir_to_file_when_an_extra_ignored_directory_holds_a_nested_repo() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-extra-nested-repo", "cache/\n");
        let cache_path = watched_path.join("thing/cache");
        std::fs::create_dir_all(&cache_path).unwrap();
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let nested = git2::Repository::init_opts(&cache_path, &init_opts).unwrap();
        std::fs::write(cache_path.join("tracked.txt"), "nested\n").unwrap();
        commit_all(&nested, "nested v1");
        drop(nested);

        let processor = processor_for(&watched_path);
        assert!(!processor._checkout_dir_is_safe(v1, v2, Path::new("thing")));
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        match processor._checkout_and_get_ref(Some(v1), v2, Some(&changed)) {
            Ok(_) => panic!(
                "checkout must refuse when an extra ignored directory holds a nested repo"
            ),
            Err(e) => assert!(
                format!("{:?}", e).contains("thing"),
                "unexpected error: {:?}",
                e
            ),
        }
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/cache/tracked.txt")).unwrap(),
            "nested\n"
        );
        assert!(watched_path.join("thing/cache/.git/HEAD").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_dir_to_file_when_an_extra_directory_is_a_gitlink_in_the_index() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-extra-gitlink-index", "cache/\n");
        std::fs::create_dir_all(watched_path.join("thing/cache")).unwrap();
        std::fs::write(watched_path.join("thing/cache/data.bin"), "ignored\n").unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        add_index_entry_at_stage(&watched, Path::new("thing/cache"), v1, 0o160000, 0);
        drop(watched);

        let processor = processor_for(&watched_path);
        assert!(!processor._checkout_dir_is_safe(v1, v2, Path::new("thing")));
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/cache/data.bin")).unwrap(),
            "ignored\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_dir_to_file_when_a_ignored_file_is_staged_after_the_index_was_read() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-extra-staged-late", "*.log\n");
        std::fs::create_dir_all(watched_path.join("thing/cache")).unwrap();
        std::fs::write(watched_path.join("thing/cache/debug.log"), "staged\n").unwrap();

        let processor = processor_for(&watched_path);
        let _ = processor.repo.index().unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let mut index = watched.index().unwrap();
        index
            .add_path(Path::new("thing/cache/debug.log"))
            .unwrap();
        index.write().unwrap();
        drop(index);
        drop(watched);

        assert!(!processor._checkout_dir_is_safe(v1, v2, Path::new("thing")));
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        match processor._checkout_and_get_ref(Some(v1), v2, Some(&changed)) {
            Ok(_) => panic!(
                "checkout must refuse when an ignored file was staged after the index was first read"
            ),
            Err(e) => assert!(
                format!("{:?}", e).contains("thing"),
                "unexpected error: {:?}",
                e
            ),
        }
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/cache/debug.log")).unwrap(),
            "staged\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_dir_to_file_when_an_extra_directory_holds_a_conflicted_ignored_file() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-extra-conflicted", "*.log\n");
        std::fs::create_dir_all(watched_path.join("thing/cache")).unwrap();
        std::fs::write(watched_path.join("thing/cache/debug.log"), "conflicted\n").unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let blob = watched.blob(b"conflicted\n").unwrap();
        add_index_entry_at_stage(
            &watched,
            Path::new("thing/cache/debug.log"),
            blob,
            0o100644,
            2,
        );
        drop(watched);

        let processor = processor_for(&watched_path);
        assert!(!processor._checkout_dir_is_safe(v1, v2, Path::new("thing")));
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/cache/debug.log")).unwrap(),
            "conflicted\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_dir_to_file_when_an_extra_ignored_directory_is_a_bare_repo() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-extra-bare-repo", "cache/\n");
        let cache_path = watched_path.join("thing/cache");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        init_opts.bare(true);
        let bare = git2::Repository::init_opts(&cache_path, &init_opts).unwrap();
        drop(bare);
        assert!(cache_path.join("HEAD").exists());

        let processor = processor_for(&watched_path);
        assert!(!processor._checkout_dir_is_safe(v1, v2, Path::new("thing")));
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        match processor._checkout_and_get_ref(Some(v1), v2, Some(&changed)) {
            Ok(_) => panic!(
                "checkout must refuse when an extra ignored directory is a bare repo"
            ),
            Err(e) => assert!(
                format!("{:?}", e).contains("thing"),
                "unexpected error: {:?}",
                e
            ),
        }
        drop(processor);
        assert!(watched_path.join("thing/cache/HEAD").exists());
        assert!(watched_path.join("thing/cache/objects").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_dir_to_file_when_a_staged_file_under_an_extra_directory_is_deleted_from_disk() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-extra-staged-deleted", "*.log\n");
        std::fs::create_dir_all(watched_path.join("thing/cache")).unwrap();
        std::fs::write(watched_path.join("thing/cache/debug.log"), "ignored\n").unwrap();
        std::fs::write(watched_path.join("thing/cache/staged.log"), "staged\n").unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let mut index = watched.index().unwrap();
        index
            .add_path(Path::new("thing/cache/staged.log"))
            .unwrap();
        index.write().unwrap();
        drop(index);
        drop(watched);
        std::fs::remove_file(watched_path.join("thing/cache/staged.log")).unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._checkout_dir_is_safe(v1, v2, Path::new("thing")));
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/cache/debug.log")).unwrap(),
            "ignored\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_dir_to_file_when_a_gitlink_under_an_extra_directory_is_missing_on_disk() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-extra-gitlink-missing", "*.log\n");
        std::fs::create_dir_all(watched_path.join("thing/cache")).unwrap();
        std::fs::write(watched_path.join("thing/cache/debug.log"), "ignored\n").unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        add_index_entry_at_stage(
            &watched,
            Path::new("thing/cache/sub"),
            v1,
            0o160000,
            0,
        );
        drop(watched);

        let processor = processor_for(&watched_path);
        assert!(!processor._checkout_dir_is_safe(v1, v2, Path::new("thing")));
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/cache/debug.log")).unwrap(),
            "ignored\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_dir_to_file_when_the_index_holds_a_file_at_the_extra_directory_path() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-extra-index-file", "cache/\n");
        std::fs::create_dir_all(watched_path.join("thing/cache")).unwrap();
        std::fs::write(watched_path.join("thing/cache/data.bin"), "ignored\n").unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let blob = watched.blob(b"tracked\n").unwrap();
        add_index_entry_at_stage(&watched, Path::new("thing/cache"), blob, 0o100644, 0);
        drop(watched);

        let processor = processor_for(&watched_path);
        assert!(!processor._checkout_dir_is_safe(v1, v2, Path::new("thing")));
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/cache/data.bin")).unwrap(),
            "ignored\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_when_the_current_commit_is_unknown() {
        let (root, watched_path, v1, v2) = two_file_repo("checkout-revalidate-unknown-local");
        let processor = processor_for(&watched_path);
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        match processor._checkout_and_get_ref(None, v2, Some(&changed)) {
            Ok(_) => panic!("checkout must refuse when the current commit is unknown"),
            Err(e) => assert!(
                format!("{:?}", e).contains("could not be determined"),
                "unexpected error: {:?}",
                e
            ),
        }
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("a.txt")).unwrap(),
            "one-a\n"
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("b.txt")).unwrap(),
            "one-b\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_dir_only_typechange_under_a_symlinked_ancestor() {
        let root = temp_dir("checkout-symlink-ancestor-dir-only");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        let sig = Signature::now("zlorb-test", "zlorb-test@example.com").unwrap();
        let keep = watched.blob(b"keep\n").unwrap();
        let empty_tree = watched.treebuilder(None).unwrap().write().unwrap();
        let thing_v1 = {
            let mut builder = watched.treebuilder(None).unwrap();
            builder.insert("empty", empty_tree, 0o40000).unwrap();
            builder.write().unwrap()
        };
        let link_v1 = {
            let mut builder = watched.treebuilder(None).unwrap();
            builder.insert("thing", thing_v1, 0o40000).unwrap();
            builder.write().unwrap()
        };
        let v1_tree = {
            let mut builder = watched.treebuilder(None).unwrap();
            builder.insert("keep.txt", keep, 0o100644).unwrap();
            builder.insert("link", link_v1, 0o40000).unwrap();
            builder.write().unwrap()
        };
        let v1_tree = watched.find_tree(v1_tree).unwrap();
        let v1 = watched
            .commit(Some("HEAD"), &sig, &sig, "v1", &v1_tree, &[])
            .unwrap();
        drop(v1_tree);
        let thing_blob = watched.blob(b"now a file\n").unwrap();
        let link_v2 = {
            let mut builder = watched.treebuilder(None).unwrap();
            builder.insert("thing", thing_blob, 0o100644).unwrap();
            builder.write().unwrap()
        };
        let v2_tree = {
            let mut builder = watched.treebuilder(None).unwrap();
            builder.insert("keep.txt", keep, 0o100644).unwrap();
            builder.insert("link", link_v2, 0o40000).unwrap();
            builder.write().unwrap()
        };
        let v2_tree = watched.find_tree(v2_tree).unwrap();
        let v1_commit = watched.find_commit(v1).unwrap();
        let v2 = watched
            .commit(Some("HEAD"), &sig, &sig, "v2", &v2_tree, &[&v1_commit])
            .unwrap();
        drop(v2_tree);
        watched
            .reset(v1_commit.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(v1_commit);
        drop(watched);
        let outside_path = root.join("outside");
        std::fs::create_dir_all(outside_path.join("thing/empty")).unwrap();
        let _ = std::fs::remove_dir_all(watched_path.join("link"));
        let _ = std::fs::remove_file(watched_path.join("link"));
        std::os::unix::fs::symlink(&outside_path, watched_path.join("link")).unwrap();

        let processor = processor_for(&watched_path);
        let changed = processor._changed_paths(v1, v2).unwrap();
        assert_eq!(changed, vec![PathBuf::from("link/thing")]);
        move_ref_to_v2(&processor, v2);
        assert!(!processor._checkout_dir_is_safe(v1, v2, Path::new("link/thing")));
        match processor._checkout_and_get_ref(Some(v1), v2, Some(&changed)) {
            Ok(_) => panic!(
                "checkout must refuse a dir-only typechange under a symlinked ancestor"
            ),
            Err(e) => assert!(
                format!("{:?}", e).contains("would be overwritten by the checkout"),
                "unexpected error: {:?}",
                e
            ),
        }
        drop(processor);
        assert!(outside_path.join("thing").is_dir());
        assert!(outside_path.join("thing/empty").is_dir());
        assert_eq!(
            std::fs::read_to_string(watched_path.join("keep.txt")).unwrap(),
            "keep\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_apply_rollback_preserves_identical_untracked_file_at_a_fetch_added_path() {
        let root = temp_dir("failed-apply-identical-untracked");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        std::fs::write(watched_path.join("new.txt"), "new file\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        let v1_commit = watched.find_commit(v1).unwrap();
        watched
            .reset(v1_commit.as_object(), git2::ResetType::Mixed, None)
            .unwrap();
        drop(v1_commit);
        drop(watched);
        write_fixture_crate(&watched_path, "fixture-version-one");

        let processor = processor_for(&watched_path);
        processor
            .repo
            .reference("refs/remotes/origin/master", v2, true, "test")
            .unwrap();
        let changed = processor._changed_paths(v1, v2).unwrap();
        {
            let fetch_ref = processor
                .repo
                .find_reference("refs/remotes/origin/master")
                .unwrap();
            let fetch_commit = processor
                .repo
                .reference_to_annotated_commit(&fetch_ref)
                .unwrap();
            match processor._apply_fast_forward(fetch_commit, v1, Some(&changed)) {
                Ok(_) => panic!("apply must refuse the untracked collision"),
                Err(e) => assert!(
                    format!("{:?}", e).contains("untracked files in the way"),
                    "unexpected error: {:?}",
                    e
                ),
            }
        }
        assert!(processor._rollback_failed_apply(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("new.txt")).unwrap(),
            "new file\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_apply_rollback_still_cleans_up_when_the_update_was_applied() {
        let root = temp_dir("failed-apply-applied-cleanup");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        std::fs::write(watched_path.join("new.txt"), "new file\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let processor = processor_for(&watched_path);
        assert!(processor._rollback_failed_apply(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert!(!watched_path.join("new.txt").exists());
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-one")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_edit_made_during_the_update_on_a_touched_file() {
        let root = temp_dir("rollback-race-edit-touched");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        std::fs::write(watched_path.join("src/main.rs"), "race edit\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("src/main.rs")).unwrap(),
            "race edit\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restores_crlf_file_holding_the_fetched_content() {
        let root = temp_dir("rollback-crlf-fetched");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::write(watched_path.join(".gitattributes"), "*.txt text eol=crlf\n")
            .unwrap();
        std::fs::write(watched_path.join("data.txt"), "line one\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::write(watched_path.join("data.txt"), "line two\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        std::fs::write(watched_path.join("data.txt"), "line two\r\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read(watched_path.join("data.txt")).unwrap(),
            b"line one\r\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_deletion_made_during_the_update_on_a_touched_file() {
        let root = temp_dir("rollback-race-delete-touched");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        std::fs::remove_file(watched_path.join("src/main.rs")).unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert!(!watched_path.join("src/main.rs").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_mode_only_edit_made_during_the_update() {
        let root = temp_dir("rollback-race-mode-touched");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                watched_path.join("src/main.rs"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(watched_path.join("src/main.rs"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-two")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn commit_with_gitlink(repo: &git2::Repository, sub_oid: Oid, message: &str) -> Oid {
        let mut index = repo.index().unwrap();
        index.clear().unwrap();
        index.add_path(Path::new("Cargo.toml")).unwrap();
        index.add_path(Path::new("src/main.rs")).unwrap();
        let entry = git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o160000,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: sub_oid,
            flags: 0,
            flags_extended: 0,
            path: b"sub".to_vec(),
        };
        index.add(&entry).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("zlorb-test", "zlorb-test@example.com").unwrap();
        let parent = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| repo.find_commit(oid).unwrap());
        match parent {
            Some(parent) => repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])
                .unwrap(),
            None => repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &[])
                .unwrap(),
        }
    }

    fn gitlink_fixture(label: &str) -> (PathBuf, PathBuf, Oid, Oid, Oid, Oid) {
        let root = temp_dir(label);
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let sub_path = watched_path.join("sub");
        let sub = git2::Repository::init_opts(&sub_path, &init_opts).unwrap();
        std::fs::write(sub_path.join(".gitignore"), "data.db\n").unwrap();
        std::fs::write(sub_path.join("sub.txt"), "sub one\n").unwrap();
        let sub_a = commit_all(&sub, "sub a");
        std::fs::write(sub_path.join("sub.txt"), "sub two\n").unwrap();
        let sub_b = commit_all(&sub, "sub b");
        let a_commit = sub.find_commit(sub_a).unwrap();
        sub.reset(a_commit.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(a_commit);
        drop(sub);
        let v1 = commit_with_gitlink(&watched, sub_a, "v1");
        let sub = git2::Repository::open(&sub_path).unwrap();
        let b_commit = sub.find_commit(sub_b).unwrap();
        sub.reset(b_commit.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(b_commit);
        drop(sub);
        let v2 = commit_with_gitlink(&watched, sub_b, "v2");
        drop(watched);
        (root, watched_path, v1, v2, sub_a, sub_b)
    }

    #[test]
    fn rollback_restores_gitlink_whose_head_matches_the_fetched_commit() {
        let (root, watched_path, v1, v2, sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-fetched");
        let processor = processor_for(&watched_path);
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        let index = processor.repo.index().unwrap();
        assert_eq!(index.get_path(Path::new("sub"), 0).unwrap().id, sub_a);
        drop(index);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("sub/sub.txt")).unwrap(),
            "sub two\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_gitlink_whose_head_matches_neither_commit() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-diverged");
        let sub_path = watched_path.join("sub");
        let sub = git2::Repository::open(&sub_path).unwrap();
        std::fs::write(sub_path.join("sub.txt"), "sub three\n").unwrap();
        let sub_c = commit_all(&sub, "sub c");
        drop(sub);

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        let sub = git2::Repository::open(&sub_path).unwrap();
        assert_eq!(head_oid(&sub), sub_c);
        drop(sub);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("sub/sub.txt")).unwrap(),
            "sub three\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_gitlink_with_uncommitted_edits() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-dirty");
        std::fs::write(watched_path.join("sub/sub.txt"), "user edit\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("sub/sub.txt")).unwrap(),
            "user edit\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_gitlink_with_an_untracked_file() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-untracked");
        std::fs::write(watched_path.join("sub/notes.txt"), "user notes\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("sub/notes.txt")).unwrap(),
            "user notes\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restores_gitlink_holding_only_an_ignored_file() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-ignored");
        std::fs::write(watched_path.join("sub/data.db"), "build output\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("sub/data.db")).unwrap(),
            "build output\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restores_uninitialized_gitlink_directory() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-uninitialized");
        std::fs::remove_dir_all(watched_path.join("sub")).unwrap();
        std::fs::create_dir(watched_path.join("sub")).unwrap();

        let processor = processor_for(&watched_path);
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restores_file_replaced_by_directory_holding_the_fetched_contents() {
        let root = temp_dir("rollback-typechange-dir");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::write(watched_path.join("thing"), "file version\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::remove_file(watched_path.join("thing")).unwrap();
        std::fs::create_dir_all(watched_path.join("thing/nested")).unwrap();
        std::fs::write(watched_path.join("thing/a.txt"), "a\n").unwrap();
        std::fs::write(watched_path.join("thing/nested/b.txt"), "b\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let processor = processor_for(&watched_path);
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing")).unwrap(),
            "file version\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restores_typechange_directory_with_a_symlink_child() {
        let root = temp_dir("rollback-typechange-dir-symlink");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::write(watched_path.join("thing"), "file version\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::remove_file(watched_path.join("thing")).unwrap();
        std::fs::create_dir_all(watched_path.join("thing/nested")).unwrap();
        std::fs::write(watched_path.join("thing/f.txt"), "f\n").unwrap();
        std::os::unix::fs::symlink("f.txt", watched_path.join("thing/link")).unwrap();
        std::fs::write(watched_path.join("thing/nested/b.txt"), "b\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let processor = processor_for(&watched_path);
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing")).unwrap(),
            "file version\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_typechange_directory_with_a_symlinked_child_directory() {
        let root = temp_dir("rollback-typechange-dir-symlink-child");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::write(watched_path.join("thing"), "file version\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::remove_file(watched_path.join("thing")).unwrap();
        std::fs::create_dir_all(watched_path.join("thing/inner")).unwrap();
        std::fs::write(watched_path.join("thing/f.txt"), "f\n").unwrap();
        std::fs::write(watched_path.join("thing/inner/b.txt"), "b\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        let outside_path = root.join("outside");
        std::fs::create_dir_all(&outside_path).unwrap();
        std::fs::write(outside_path.join("b.txt"), "b\n").unwrap();
        std::fs::remove_dir_all(watched_path.join("thing/inner")).unwrap();
        std::os::unix::fs::symlink(&outside_path, watched_path.join("thing/inner")).unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert!(
            std::fs::symlink_metadata(watched_path.join("thing/inner"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(outside_path.join("b.txt")).unwrap(),
            "b\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_file_under_a_symlinked_ancestor_directory() {
        let root = temp_dir("rollback-symlink-ancestor-file");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::create_dir_all(watched_path.join("parent/mid")).unwrap();
        std::fs::write(watched_path.join("parent/mid/thing.txt"), "one\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::write(watched_path.join("parent/mid/thing.txt"), "two\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        let outside_path = root.join("outside");
        std::fs::create_dir_all(&outside_path).unwrap();
        std::fs::write(outside_path.join("thing.txt"), "two\n").unwrap();
        std::fs::remove_dir_all(watched_path.join("parent/mid")).unwrap();
        std::os::unix::fs::symlink(&outside_path, watched_path.join("parent/mid")).unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert!(
            std::fs::symlink_metadata(watched_path.join("parent/mid"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(outside_path.join("thing.txt")).unwrap(),
            "two\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_typechange_directory_under_a_symlinked_ancestor() {
        let root = temp_dir("rollback-symlink-ancestor-typechange");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::create_dir_all(watched_path.join("parent")).unwrap();
        std::fs::write(watched_path.join("parent/thing"), "file version\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::remove_file(watched_path.join("parent/thing")).unwrap();
        std::fs::create_dir_all(watched_path.join("parent/thing")).unwrap();
        std::fs::write(watched_path.join("parent/thing/a.txt"), "a\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        let outside_path = root.join("outside");
        std::fs::create_dir_all(outside_path.join("thing")).unwrap();
        std::fs::write(outside_path.join("thing/a.txt"), "a\n").unwrap();
        std::fs::remove_dir_all(watched_path.join("parent")).unwrap();
        std::os::unix::fs::symlink(&outside_path, watched_path.join("parent")).unwrap();

        let processor = processor_for(&watched_path);
        let changed = processor._changed_paths(v1, v2).unwrap();
        let (safe, preserved, _) = processor._rollback_restore_paths(v1, v2, &changed);
        assert!(preserved.contains(&PathBuf::from("parent/thing")));
        assert!(preserved.contains(&PathBuf::from("parent/thing/a.txt")));
        assert!(!safe.contains(&PathBuf::from("parent/thing")));
        assert!(!safe.contains(&PathBuf::from("parent/thing/a.txt")));
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert!(
            std::fs::symlink_metadata(watched_path.join("parent"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(outside_path.join("thing/a.txt")).unwrap(),
            "a\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restore_paths_preserves_everything_when_a_commit_cannot_be_loaded() {
        let root = temp_dir("rollback-restore-paths-unloadable-commit");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::create_dir_all(watched_path.join("parent/mid")).unwrap();
        std::fs::write(watched_path.join("parent/mid/thing.txt"), "one\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::write(watched_path.join("parent/mid/thing.txt"), "two\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        let bogus = Oid::from_str("1111111111111111111111111111111111111111").unwrap();
        let changed = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("parent/mid/thing.txt"),
        ];

        let processor = processor_for(&watched_path);
        let (safe, preserved, failed) = processor._rollback_restore_paths(v1, bogus, &changed);
        assert!(safe.is_empty());
        assert_eq!(preserved, changed);
        assert!(failed);
        let (safe, preserved, failed) = processor._rollback_restore_paths(bogus, v2, &changed);
        assert!(safe.is_empty());
        assert_eq!(preserved, changed);
        assert!(failed);
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restore_paths_preserves_everything_when_a_tree_cannot_be_loaded() {
        let root = temp_dir("rollback-restore-paths-unloadable-tree");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        let odb = watched.odb().unwrap();
        let broken = odb
            .write(
                git2::ObjectType::Commit,
                b"tree 1111111111111111111111111111111111111111\nauthor A <a@a.invalid> 0 +0000\ncommitter A <a@a.invalid> 0 +0000\n\nbroken\n",
            )
            .unwrap();
        drop(odb);
        drop(watched);

        let processor = processor_for(&watched_path);
        assert!(processor.repo.find_commit(broken).is_ok());
        let changed = vec![PathBuf::from("src/main.rs")];
        let (safe, preserved, failed) = processor._rollback_restore_paths(broken, v1, &changed);
        assert!(safe.is_empty());
        assert_eq!(preserved, changed);
        assert!(failed);
        let (safe, preserved, failed) = processor._rollback_restore_paths(v1, broken, &changed);
        assert!(safe.is_empty());
        assert_eq!(preserved, changed);
        assert!(failed);
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restore_paths_preserves_everything_when_the_repo_has_no_workdir() {
        let root = temp_dir("rollback-restore-paths-no-workdir");
        let source_path = root.join("source");
        let bare_path = root.join("bare");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let source = git2::Repository::init_opts(&source_path, &init_opts).unwrap();
        write_fixture_crate(&source_path, "fixture-version-one");
        let v1 = commit_all(&source, "v1");
        std::fs::write(source_path.join("src/main.rs"), "fn main() {}\n").unwrap();
        let v2 = commit_all(&source, "v2");
        drop(source);
        let status = std::process::Command::new("git")
            .args(["clone", "--bare"])
            .arg(&source_path)
            .arg(&bare_path)
            .status()
            .unwrap();
        assert!(status.success());
        let bare = git2::Repository::open(&bare_path).unwrap();
        assert!(bare.workdir().is_none());
        assert!(bare.find_commit(v1).unwrap().tree().is_ok());
        assert!(bare.find_commit(v2).unwrap().tree().is_ok());
        drop(bare);

        let processor = processor_for(&bare_path);
        let changed = vec![PathBuf::from("src/main.rs")];
        let (safe, preserved, failed) = processor._rollback_restore_paths(v1, v2, &changed);
        assert!(safe.is_empty());
        assert_eq!(preserved, changed);
        assert!(failed);
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_refuses_with_a_classification_message_when_a_commit_cannot_be_loaded() {
        let (root, watched_path, v1, v2) = two_file_repo("checkout-classification-failed");
        let processor = processor_for(&watched_path);
        move_ref_to_v2(&processor, v2);
        let changed = processor._changed_paths(v1, v2).unwrap();
        let bogus = Oid::from_str("1111111111111111111111111111111111111111").unwrap();
        match processor._checkout_and_get_ref(Some(v1), bogus, Some(&changed)) {
            Ok(_) => panic!("checkout must refuse when classification cannot load a commit"),
            Err(e) => assert!(
                format!("{:?}", e).contains("could not be classified"),
                "unexpected error: {:?}",
                e
            ),
        }
        assert_eq!(
            std::fs::read_to_string(watched_path.join("a.txt")).unwrap(),
            "one-a\n"
        );
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkout_step_does_not_promote_a_directory_when_classification_failed() {
        let (root, watched_path, v1, v2) =
            dir_to_file_repo_with_gitignore("checkout-dir-no-promotion-on-load-failure", "*.log\n");
        let watched = git2::Repository::open(&watched_path).unwrap();
        let odb = watched.odb().unwrap();
        let broken = odb
            .write(
                git2::ObjectType::Commit,
                b"tree 1111111111111111111111111111111111111111\nauthor A <a@a.invalid> 0 +0000\ncommitter A <a@a.invalid> 0 +0000\n\nbroken\n",
            )
            .unwrap();
        drop(odb);
        drop(watched);

        let processor = processor_for(&watched_path);
        assert!(processor._checkout_dir_is_safe(v1, broken, Path::new("thing")));
        move_ref_to_v2(&processor, v2);
        let changed = vec![PathBuf::from("thing")];
        match processor._checkout_and_get_ref(Some(v1), broken, Some(&changed)) {
            Ok(_) => panic!("checkout must refuse when classification cannot load a tree"),
            Err(e) => assert!(
                format!("{:?}", e).contains("could not be classified"),
                "unexpected error: {:?}",
                e
            ),
        }
        drop(processor);
        assert!(watched_path.join("thing").is_dir());
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/inner.txt")).unwrap(),
            "inner\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restore_paths_checks_disk_for_a_path_identical_in_both_trees() {
        let root = temp_dir("rollback-restore-paths-identical-entry");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::write(watched_path.join("stable.txt"), "stable\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let paths = vec![PathBuf::from("stable.txt")];
        let processor = processor_for(&watched_path);
        let (safe, preserved, _) = processor._rollback_restore_paths(v1, v2, &paths);
        assert_eq!(safe, paths);
        assert!(preserved.is_empty());
        std::fs::write(watched_path.join("stable.txt"), "edited\n").unwrap();
        let (safe, preserved, _) = processor._rollback_restore_paths(v1, v2, &paths);
        assert!(safe.is_empty());
        assert_eq!(preserved, paths);
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restore_paths_checks_disk_for_a_directory_identical_in_both_trees() {
        let root = temp_dir("rollback-restore-paths-identical-dir");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::create_dir_all(watched_path.join("sub")).unwrap();
        std::fs::write(watched_path.join("sub/a.txt"), "a\n").unwrap();
        std::fs::write(watched_path.join("sub/b.txt"), "b\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let paths = vec![PathBuf::from("sub")];
        let processor = processor_for(&watched_path);
        let (safe, preserved, _) = processor._rollback_restore_paths(v1, v2, &paths);
        assert_eq!(safe, paths);
        assert!(preserved.is_empty());
        std::fs::write(watched_path.join("sub/extra.txt"), "extra\n").unwrap();
        let (safe, preserved, _) = processor._rollback_restore_paths(v1, v2, &paths);
        assert!(safe.is_empty());
        assert_eq!(preserved, paths);
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restore_paths_checks_disk_for_a_gitlink_identical_in_both_trees() {
        let root = temp_dir("rollback-restore-paths-identical-gitlink");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let sub_path = watched_path.join("sub");
        let sub = git2::Repository::init_opts(&sub_path, &init_opts).unwrap();
        std::fs::write(sub_path.join("sub.txt"), "sub one\n").unwrap();
        let sub_a = commit_all(&sub, "sub a");
        drop(sub);
        let v1 = commit_with_gitlink(&watched, sub_a, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_with_gitlink(&watched, sub_a, "v2");
        drop(watched);

        let paths = vec![PathBuf::from("sub")];
        let processor = processor_for(&watched_path);
        let (safe, preserved, _) = processor._rollback_restore_paths(v1, v2, &paths);
        assert_eq!(safe, paths);
        assert!(preserved.is_empty());
        std::fs::write(watched_path.join("sub/notes.txt"), "user notes\n").unwrap();
        let (safe, preserved, _) = processor._rollback_restore_paths(v1, v2, &paths);
        assert!(safe.is_empty());
        assert_eq!(preserved, paths);
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_whole_tree_fallback_does_not_write_through_a_symlinked_ancestor() {
        let root = temp_dir("rollback-whole-tree-fallback-symlink");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::create_dir_all(watched_path.join("parent/mid")).unwrap();
        std::fs::write(watched_path.join("parent/mid/thing.txt"), "one\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        std::fs::write(watched_path.join("parent/mid/thing.txt"), "two\n").unwrap();
        let _v2 = commit_all(&watched, "v2");
        drop(watched);
        let outside_path = root.join("outside");
        std::fs::create_dir_all(&outside_path).unwrap();
        std::fs::write(outside_path.join("thing.txt"), "two\n").unwrap();
        std::fs::remove_dir_all(watched_path.join("parent/mid")).unwrap();
        std::os::unix::fs::symlink(&outside_path, watched_path.join("parent/mid")).unwrap();
        let bogus = Oid::from_str("1111111111111111111111111111111111111111").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, bogus));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-two")
        );
        assert!(
            std::fs::symlink_metadata(watched_path.join("parent/mid"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(outside_path.join("thing.txt")).unwrap(),
            "two\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_typechange_directory_holding_a_symlinked_nested_gitlink() {
        let (root, watched_path, v1, v2) =
            nested_gitlink_fixture("rollback-nested-gitlink-symlink");
        std::fs::remove_dir_all(watched_path.join("thing/sub")).unwrap();
        std::os::unix::fs::symlink(root.join("sub-src"), watched_path.join("thing/sub"))
            .unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert!(
            std::fs::symlink_metadata(watched_path.join("thing/sub"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(root.join("sub-src/s.txt")).unwrap(),
            "sub two\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restores_typechange_directory_matching_the_local_tree() {
        let root = temp_dir("rollback-typechange-dir-local");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::create_dir_all(watched_path.join("thing")).unwrap();
        std::fs::write(watched_path.join("thing/a.txt"), "a\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::remove_dir_all(watched_path.join("thing")).unwrap();
        std::fs::write(watched_path.join("thing"), "file version\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        std::fs::remove_file(watched_path.join("thing")).unwrap();
        std::fs::create_dir_all(watched_path.join("thing")).unwrap();
        std::fs::write(watched_path.join("thing/a.txt"), "a\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/a.txt")).unwrap(),
            "a\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_commitless_repo_at_a_gitlink_path() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-commitless");
        std::fs::remove_dir_all(watched_path.join("sub")).unwrap();
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let fresh = git2::Repository::init_opts(watched_path.join("sub"), &init_opts).unwrap();
        drop(fresh);
        std::fs::write(watched_path.join("sub/draft.txt"), "draft\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("sub/draft.txt")).unwrap(),
            "draft\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_commitless_repo_holding_only_dot_git_at_a_gitlink_path() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-commitless-only-dot-git");
        std::fs::remove_dir_all(watched_path.join("sub")).unwrap();
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let fresh = git2::Repository::init_opts(watched_path.join("sub"), &init_opts).unwrap();
        drop(fresh);

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        let sub = git2::Repository::open(watched_path.join("sub")).unwrap();
        assert!(sub.head().ok().and_then(|head| head.target()).is_none());
        drop(sub);
        let names: Vec<_> = std::fs::read_dir(watched_path.join("sub"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(".git")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn bare_clone_at_gitlink(watched_path: &Path, root: &Path) {
        let bare_tmp = root.join("bare-clone");
        let status = std::process::Command::new("git")
            .args(["clone", "--bare"])
            .arg(watched_path.join("sub"))
            .arg(&bare_tmp)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::remove_dir_all(watched_path.join("sub")).unwrap();
        std::fs::rename(&bare_tmp, watched_path.join("sub")).unwrap();
    }

    #[test]
    fn rollback_preserves_bare_repo_whose_head_matches_the_fetched_commit() {
        let (root, watched_path, v1, v2, sub_a, sub_b) =
            gitlink_fixture("rollback-gitlink-bare-fetched");
        bare_clone_at_gitlink(&watched_path, &root);
        let bare = git2::Repository::open(watched_path.join("sub")).unwrap();
        assert!(bare.is_bare());
        assert_eq!(head_oid(&bare), sub_b);
        bare.reference("refs/heads/user-work", sub_a, false, "test")
            .unwrap();
        drop(bare);

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        let bare = git2::Repository::open(watched_path.join("sub")).unwrap();
        assert!(bare.is_bare());
        assert_eq!(head_oid(&bare), sub_b);
        assert_eq!(
            bare.find_reference("refs/heads/user-work")
                .unwrap()
                .target(),
            Some(sub_a)
        );
        drop(bare);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_bare_repo_whose_head_matches_the_local_commit() {
        let (root, watched_path, v1, v2, sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-bare-local");
        bare_clone_at_gitlink(&watched_path, &root);
        let bare = git2::Repository::open(watched_path.join("sub")).unwrap();
        assert!(bare.is_bare());
        bare.set_head_detached(sub_a).unwrap();
        assert_eq!(head_oid(&bare), sub_a);
        drop(bare);

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        let bare = git2::Repository::open(watched_path.join("sub")).unwrap();
        assert!(bare.is_bare());
        assert_eq!(head_oid(&bare), sub_a);
        drop(bare);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_commitless_bare_repo_at_a_gitlink_path() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-bare-commitless");
        std::fs::remove_dir_all(watched_path.join("sub")).unwrap();
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        init_opts.bare(true);
        let bare = git2::Repository::init_opts(watched_path.join("sub"), &init_opts).unwrap();
        assert!(bare.is_bare());
        drop(bare);

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        let bare = git2::Repository::open(watched_path.join("sub")).unwrap();
        assert!(bare.is_bare());
        assert!(bare.head().ok().and_then(|head| head.target()).is_none());
        drop(bare);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn submodule_placeholder(watched_path: &Path) {
        std::fs::remove_dir_all(watched_path.join("sub")).unwrap();
        let gitdir = watched_path.join(".git/modules/sub");
        std::fs::create_dir_all(watched_path.join(".git/modules")).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-b", "master", "--separate-git-dir"])
            .arg(&gitdir)
            .arg(watched_path.join("sub"))
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn rollback_restores_submodule_placeholder_at_a_gitlink_path() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-submodule-placeholder");
        submodule_placeholder(&watched_path);
        assert!(watched_path.join("sub/.git").is_file());

        let processor = processor_for(&watched_path);
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_submodule_placeholder_holding_an_object() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-placeholder-with-object");
        submodule_placeholder(&watched_path);
        let placeholder = git2::Repository::open(watched_path.join("sub")).unwrap();
        let blob = placeholder.blob(b"user object\n").unwrap();
        drop(placeholder);

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        let placeholder = git2::Repository::open(watched_path.join("sub")).unwrap();
        assert!(placeholder.find_blob(blob).is_ok());
        drop(placeholder);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_submodule_placeholder_with_an_ignored_file() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-placeholder-ignored");
        submodule_placeholder(&watched_path);
        std::fs::write(
            watched_path.join(".git/modules/sub/info/exclude"),
            "data.db\n",
        )
        .unwrap();
        std::fs::write(watched_path.join("sub/data.db"), "user data\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("sub/data.db")).unwrap(),
            "user data\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restores_submodule_placeholder_holding_only_empty_directories() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-placeholder-empty-dirs");
        submodule_placeholder(&watched_path);
        std::fs::create_dir_all(watched_path.join("sub/empty/nested")).unwrap();

        let processor = processor_for(&watched_path);
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_submodule_placeholder_with_a_file_in_an_ignored_directory() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-placeholder-ignored-dir-file");
        submodule_placeholder(&watched_path);
        std::fs::write(
            watched_path.join(".git/modules/sub/info/exclude"),
            "logs/\n",
        )
        .unwrap();
        std::fs::create_dir_all(watched_path.join("sub/logs")).unwrap();
        std::fs::write(watched_path.join("sub/logs/x.txt"), "user log\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("sub/logs/x.txt")).unwrap(),
            "user log\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_submodule_placeholder_with_an_untracked_file() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-placeholder-untracked");
        submodule_placeholder(&watched_path);
        std::fs::write(watched_path.join("sub/draft.txt"), "draft\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("sub/draft.txt")).unwrap(),
            "draft\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_submodule_placeholder_with_a_redirected_worktree() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-gitlink-placeholder-redirected-worktree");
        submodule_placeholder(&watched_path);
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        set_repo_config(
            &watched_path.join("sub"),
            "core.worktree",
            root.join("elsewhere").to_string_lossy().as_ref(),
        );
        std::fs::write(watched_path.join("sub/draft.txt"), "draft\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("sub/draft.txt")).unwrap(),
            "draft\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_typechange_directory_holding_a_commitless_nested_gitlink() {
        let (root, watched_path, v1, v2) =
            nested_gitlink_fixture("rollback-nested-gitlink-commitless");
        std::fs::remove_dir_all(watched_path.join("thing/sub")).unwrap();
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let fresh =
            git2::Repository::init_opts(watched_path.join("thing/sub"), &init_opts).unwrap();
        drop(fresh);

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        let sub = git2::Repository::open(watched_path.join("thing/sub")).unwrap();
        assert!(sub.head().ok().and_then(|head| head.target()).is_none());
        drop(sub);
        let names: Vec<_> = std::fs::read_dir(watched_path.join("thing/sub"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(".git")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn nested_gitlink_fixture(label: &str) -> (PathBuf, PathBuf, Oid, Oid) {
        let root = temp_dir(label);
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::write(watched_path.join("thing"), "file version\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        let sub_src_path = root.join("sub-src");
        let sub_src = git2::Repository::init_opts(&sub_src_path, &init_opts).unwrap();
        std::fs::write(sub_src_path.join(".gitignore"), "data.db\n").unwrap();
        std::fs::write(sub_src_path.join("s.txt"), "sub one\n").unwrap();
        commit_all(&sub_src, "sub a");
        std::fs::write(sub_src_path.join("s.txt"), "sub two\n").unwrap();
        let sub_b = commit_all(&sub_src, "sub b");
        drop(sub_src);
        let v2 = {
            let a_blob = watched.blob(b"a\n").unwrap();
            let mut thing_builder = watched.treebuilder(None).unwrap();
            thing_builder.insert("a.txt", a_blob, 0o100644).unwrap();
            thing_builder.insert("sub", sub_b, 0o160000).unwrap();
            let thing_tree = thing_builder.write().unwrap();
            let v1_commit = watched.find_commit(v1).unwrap();
            let v1_tree = v1_commit.tree().unwrap();
            let mut root_builder = watched.treebuilder(Some(&v1_tree)).unwrap();
            root_builder.insert("thing", thing_tree, 0o40000).unwrap();
            let v2_tree_oid = root_builder.write().unwrap();
            let v2_tree = watched.find_tree(v2_tree_oid).unwrap();
            let sig = Signature::now("zlorb-test", "zlorb-test@example.com").unwrap();
            let v2 = watched
                .commit(Some("HEAD"), &sig, &sig, "v2", &v2_tree, &[&v1_commit])
                .unwrap();
            let mut index = watched.index().unwrap();
            index.read_tree(&v2_tree).unwrap();
            index.write().unwrap();
            v2
        };
        drop(watched);
        std::fs::remove_file(watched_path.join("thing")).unwrap();
        std::fs::create_dir_all(watched_path.join("thing")).unwrap();
        std::fs::write(watched_path.join("thing/a.txt"), "a\n").unwrap();
        git2::Repository::clone(
            sub_src_path.to_string_lossy().as_ref(),
            watched_path.join("thing/sub"),
        )
        .unwrap();
        (root, watched_path, v1, v2)
    }

    #[test]
    fn rollback_restores_typechange_directory_holding_a_clean_nested_gitlink() {
        let (root, watched_path, v1, v2) =
            nested_gitlink_fixture("rollback-nested-gitlink-clean");
        let processor = processor_for(&watched_path);
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing")).unwrap(),
            "file version\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_typechange_directory_holding_a_dirty_nested_gitlink() {
        let (root, watched_path, v1, v2) =
            nested_gitlink_fixture("rollback-nested-gitlink-dirty");
        std::fs::write(watched_path.join("thing/sub/precious.txt"), "precious\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/sub/precious.txt")).unwrap(),
            "precious\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_typechange_directory_holding_a_nested_gitlink_with_an_ignored_file() {
        let (root, watched_path, v1, v2) =
            nested_gitlink_fixture("rollback-nested-gitlink-ignored");
        std::fs::write(watched_path.join("thing/sub/data.db"), "precious\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/sub/data.db")).unwrap(),
            "precious\n"
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/sub/s.txt")).unwrap(),
            "sub two\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_gitlink_replaced_by_a_file_holding_an_ignored_file() {
        let root = temp_dir("rollback-gitlink-to-file-ignored");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let sub_path = watched_path.join("sub");
        let sub = git2::Repository::init_opts(&sub_path, &init_opts).unwrap();
        std::fs::write(sub_path.join(".gitignore"), "data.db\n").unwrap();
        std::fs::write(sub_path.join("sub.txt"), "sub one\n").unwrap();
        let sub_a = commit_all(&sub, "sub a");
        drop(sub);
        let v1 = commit_with_gitlink(&watched, sub_a, "v1");
        let v2 = {
            let blob = watched.blob(b"file version\n").unwrap();
            let v1_commit = watched.find_commit(v1).unwrap();
            let v1_tree = v1_commit.tree().unwrap();
            let mut builder = watched.treebuilder(Some(&v1_tree)).unwrap();
            builder.insert("sub", blob, 0o100644).unwrap();
            let v2_tree_oid = builder.write().unwrap();
            let v2_tree = watched.find_tree(v2_tree_oid).unwrap();
            let sig = Signature::now("zlorb-test", "zlorb-test@example.com").unwrap();
            watched
                .commit(Some("HEAD"), &sig, &sig, "v2", &v2_tree, &[&v1_commit])
                .unwrap()
        };
        drop(watched);
        std::fs::write(sub_path.join("data.db"), "precious\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(sub_path.join("data.db")).unwrap(),
            "precious\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_preserves_directory_holding_a_file_the_update_did_not_create() {
        let root = temp_dir("rollback-typechange-dir-extra");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::write(watched_path.join("thing"), "file version\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::remove_file(watched_path.join("thing")).unwrap();
        std::fs::create_dir_all(watched_path.join("thing")).unwrap();
        std::fs::write(watched_path.join("thing/a.txt"), "a\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        std::fs::write(watched_path.join("thing/precious.txt"), "precious\n").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/precious.txt")).unwrap(),
            "precious\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn glob_characters_in_a_changed_filename_do_not_expand_to_sibling_files() {
        let root = temp_dir("glob-filename");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::write(upstream_path.join("data[1].txt"), "glob one\n").unwrap();
        std::fs::write(upstream_path.join("data1.txt"), "sibling base\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        std::fs::write(upstream_path.join("data[1].txt"), "glob two\n").unwrap();
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        std::fs::write(watched_path.join("data1.txt"), "sibling wip\n").unwrap();

        processor_for(&watched_path).update_from_remote().unwrap();

        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("data[1].txt")).unwrap(),
            "glob two\n"
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("data1.txt")).unwrap(),
            "sibling wip\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn commit_all_no_delete(repo: &git2::Repository, message: &str) -> Oid {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("zlorb-test", "zlorb-test@example.com").unwrap();
        let parent = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| repo.find_commit(oid).unwrap());
        match parent {
            Some(parent) => repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])
                .unwrap(),
            None => repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &[])
                .unwrap(),
        }
    }

    #[test]
    fn non_utf8_filename_in_the_update_still_refuses_when_dirty() {
        use std::os::unix::ffi::OsStringExt;
        let root = temp_dir("non-utf8-guard");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        let bad_name = std::ffi::OsString::from_vec(b"bad-\xff.txt".to_vec());
        std::fs::write(upstream_path.join(&bad_name), "version one\n").unwrap();
        let v1 = commit_all_no_delete(&upstream, "v1");
        std::fs::write(upstream_path.join(&bad_name), "version two\n").unwrap();
        commit_all_no_delete(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        std::fs::write(watched_path.join(&bad_name), "local wip\n").unwrap();

        let err = processor_for(&watched_path)
            .update_from_remote()
            .unwrap_err();
        assert!(format!("{:?}", err).contains("local changes to tracked files"));
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        assert_eq!(
            std::fs::read(watched_path.join(&bad_name)).unwrap(),
            b"local wip\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn untracked_file_in_the_way_refuses_fast_forward_until_removed() {
        let root = temp_dir("untracked-in-the-way");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        let v1 = commit_all(&upstream, "v1");
        std::fs::write(upstream_path.join("blocked.txt"), "from remote\n").unwrap();
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        std::fs::create_dir_all(watched_path.join("blocked.txt")).unwrap();
        std::fs::write(watched_path.join("blocked.txt/inner.txt"), "local\n").unwrap();

        let err = processor_for(&watched_path)
            .update_from_remote()
            .unwrap_err();
        assert!(format!("{:?}", err).contains("untracked files in the way"));
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("blocked.txt/inner.txt")).unwrap(),
            "local\n"
        );
        drop(watched);

        std::fs::remove_dir_all(watched_path.join("blocked.txt")).unwrap();
        processor_for(&watched_path).update_from_remote().unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("blocked.txt")).unwrap(),
            "from remote\n"
        );
        drop(watched);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_to_file_typechange_fast_forwards_on_clean_tree() {
        let root = temp_dir("dir-to-file-typechange");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::create_dir_all(upstream_path.join("thing")).unwrap();
        std::fs::write(upstream_path.join("thing/inner.txt"), "inner\n").unwrap();
        std::fs::write(upstream_path.join("thing/other.txt"), "other\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        std::fs::remove_dir_all(upstream_path.join("thing")).unwrap();
        std::fs::write(upstream_path.join("thing"), "now a file\n").unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        assert!(watched_path.join("thing").is_dir());

        processor_for(&watched_path).update_from_remote().unwrap();

        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert!(watched_path.join("thing").is_file());
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing")).unwrap(),
            "now a file\n"
        );
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-two")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn untracked_file_inside_tracked_directory_still_refuses_typechange() {
        let root = temp_dir("dir-to-file-untracked-inside");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::create_dir_all(upstream_path.join("thing")).unwrap();
        std::fs::write(upstream_path.join("thing/inner.txt"), "inner\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        std::fs::remove_dir_all(upstream_path.join("thing")).unwrap();
        std::fs::write(upstream_path.join("thing"), "now a file\n").unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        std::fs::write(watched_path.join("thing/stray.txt"), "untracked\n").unwrap();

        let err = processor_for(&watched_path)
            .update_from_remote()
            .unwrap_err();
        assert!(format!("{:?}", err).contains("untracked files in the way"));
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/stray.txt")).unwrap(),
            "untracked\n"
        );

        std::fs::remove_file(watched_path.join("thing/stray.txt")).unwrap();
        processor_for(&watched_path).update_from_remote().unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert!(watched_path.join("thing").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn staged_addition_inside_tracked_directory_still_refuses_typechange() {
        let root = temp_dir("dir-to-file-staged-inside");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::create_dir_all(upstream_path.join("thing")).unwrap();
        std::fs::write(upstream_path.join("thing/inner.txt"), "inner\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        std::fs::remove_dir_all(upstream_path.join("thing")).unwrap();
        std::fs::write(upstream_path.join("thing"), "now a file\n").unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-two");
        commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        std::fs::write(watched_path.join("thing/staged.txt"), "staged\n").unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let mut index = watched.index().unwrap();
        index
            .add_path(Path::new("thing/staged.txt"))
            .unwrap();
        index.write().unwrap();
        drop(index);
        drop(watched);

        let err = processor_for(&watched_path)
            .update_from_remote()
            .unwrap_err();
        assert!(format!("{:?}", err).contains("untracked files in the way"));
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing/staged.txt")).unwrap(),
            "staged\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_to_file_typechange_with_empty_subdirectory_fast_forwards() {
        let root = temp_dir("dir-to-file-empty-subdir");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::create_dir_all(upstream_path.join("thing")).unwrap();
        std::fs::write(upstream_path.join("thing/inner.txt"), "inner\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        std::fs::remove_dir_all(upstream_path.join("thing")).unwrap();
        std::fs::write(upstream_path.join("thing"), "now a file\n").unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        std::fs::create_dir_all(watched_path.join("thing/empty")).unwrap();

        processor_for(&watched_path).update_from_remote().unwrap();

        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert!(watched_path.join("thing").is_file());
        assert_eq!(
            std::fs::read_to_string(watched_path.join("thing")).unwrap(),
            "now a file\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_directory_to_file_typechange_fast_forwards() {
        let root = temp_dir("nested-dir-to-file");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        std::fs::create_dir_all(upstream_path.join("a/b")).unwrap();
        std::fs::write(upstream_path.join("a/b/inner.txt"), "inner\n").unwrap();
        std::fs::write(upstream_path.join("a/keep.txt"), "keep\n").unwrap();
        let v1 = commit_all(&upstream, "v1");
        std::fs::remove_dir_all(upstream_path.join("a/b")).unwrap();
        std::fs::write(upstream_path.join("a/b"), "now a file\n").unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);

        processor_for(&watched_path).update_from_remote().unwrap();

        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert!(watched_path.join("a/b").is_file());
        assert_eq!(
            std::fs::read_to_string(watched_path.join("a/b")).unwrap(),
            "now a file\n"
        );
        assert_eq!(
            std::fs::read_to_string(watched_path.join("a/keep.txt")).unwrap(),
            "keep\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn processor_with_tracker(watched: &Path, failed_builds: FailedBuilds) -> RepoProcessor {
        let mut processor = processor_for(watched);
        processor.failed_builds = failed_builds;
        processor
    }

    fn write_sentinel_fixture_crate(repo_path: &Path, marker: &str) {
        write_fixture_crate(repo_path, marker);
        std::fs::write(
            repo_path.join("build.rs"),
            "fn main() {\n    let sentinel = std::path::Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"fail-build\");\n    println!(\"cargo:rerun-if-changed=fail-build\");\n    if sentinel.exists() {\n        panic!(\"simulated transient build failure\");\n    }\n}\n",
        )
        .unwrap();
    }

    fn processor_with_strategy(watched: &Path, strategy: Option<&str>) -> RepoProcessor {
        RepoProcessor::new(RepositoryConfiguration {
            name: "watched".to_string(),
            path: watched.to_string_lossy().into_owned(),
            remote: "origin".to_string(),
            branch: "master".to_string(),
            build_strategy: strategy.map(|s| s.to_string()),
            build_command: String::new(),
            restart_command: None,
            out_dir: None,
        })
        .unwrap()
    }

    #[test]
    fn invalid_strategy_refuses_update_until_config_is_fixed() {
        let root = temp_dir("invalid-strategy");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        let v1 = commit_all(&upstream, "v1");
        write_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);

        let err = processor_with_strategy(&watched_path, None)
            .update_from_remote()
            .unwrap_err();
        assert!(format!("{:?}", err).contains("no build_strategy"));
        let err = processor_with_strategy(&watched_path, Some("webpack"))
            .update_from_remote()
            .unwrap_err();
        assert!(format!("{:?}", err).contains("webpack"));
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-one")
        );
        assert!(!fixture_artifact(&watched_path).exists());

        processor_with_strategy(&watched_path, Some("rust"))
            .update_from_remote()
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        let artifact = fixture_artifact(&watched_path);
        let output = std::process::Command::new(&artifact).output().unwrap();
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("fixture-version-two")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_checkout_rolls_branch_back_and_next_cycle_retries() {
        let root = temp_dir("failed-checkout");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        let v1 = commit_all(&upstream, "v1");
        write_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);
        std::fs::write(watched_path.join(".git/index.lock"), "").unwrap();

        assert!(processor_for(&watched_path).update_from_remote().is_err());
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-one")
        );

        std::fs::remove_file(watched_path.join(".git/index.lock")).unwrap();
        processor_for(&watched_path).update_from_remote().unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-two")
        );
        drop(watched);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_cleanup_removes_matching_symlink_and_skips_fifo() {
        let root = temp_dir("rollback-cleanup-types");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        std::fs::write(watched_path.join("new.txt"), "new file\n").unwrap();
        std::os::unix::fs::symlink("src/main.rs", watched_path.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("Cargo.toml", watched_path.join("stale-link.txt")).unwrap();
        std::fs::write(watched_path.join("fifo.txt"), "fifo content\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        let v1_commit = watched.find_commit(v1).unwrap();
        watched
            .reset(v1_commit.as_object(), git2::ResetType::Mixed, None)
            .unwrap();
        drop(v1_commit);
        drop(watched);
        std::fs::remove_file(watched_path.join("stale-link.txt")).unwrap();
        std::os::unix::fs::symlink("src/main.rs", watched_path.join("stale-link.txt")).unwrap();
        std::fs::remove_file(watched_path.join("fifo.txt")).unwrap();
        let mkfifo = std::process::Command::new("mkfifo")
            .arg(watched_path.join("fifo.txt"))
            .status()
            .unwrap();
        assert!(mkfifo.success());

        processor_for(&watched_path)._remove_partial_update_files(v1, v2);

        assert!(!watched_path.join("new.txt").exists());
        assert!(std::fs::symlink_metadata(watched_path.join("link.txt")).is_err());
        assert!(
            std::fs::symlink_metadata(watched_path.join("stale-link.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        use std::os::unix::fs::FileTypeExt;
        assert!(
            std::fs::symlink_metadata(watched_path.join("fifo.txt"))
                .unwrap()
                .file_type()
                .is_fifo()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_cleanup_skips_fetch_added_file_under_a_symlinked_ancestor() {
        let root = temp_dir("rollback-cleanup-symlink-ancestor");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        std::fs::create_dir_all(watched_path.join("shop")).unwrap();
        std::fs::write(watched_path.join("shop/new.txt"), "new file\n").unwrap();
        std::fs::write(watched_path.join("top.txt"), "top file\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        let v1_commit = watched.find_commit(v1).unwrap();
        watched
            .reset(v1_commit.as_object(), git2::ResetType::Mixed, None)
            .unwrap();
        drop(v1_commit);
        drop(watched);
        let outside_path = root.join("outside");
        std::fs::create_dir_all(&outside_path).unwrap();
        std::fs::write(outside_path.join("new.txt"), "new file\n").unwrap();
        std::fs::remove_dir_all(watched_path.join("shop")).unwrap();
        std::os::unix::fs::symlink(&outside_path, watched_path.join("shop")).unwrap();

        processor_for(&watched_path)._remove_partial_update_files(v1, v2);

        assert!(
            std::fs::symlink_metadata(watched_path.join("shop"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(outside_path.join("new.txt")).unwrap(),
            "new file\n"
        );
        assert!(!watched_path.join("top.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_cleanup_skips_files_under_a_symlinked_ancestor_that_is_a_local_symlink_blob() {
        let root = temp_dir("rollback-cleanup-symlink-blob-dir");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let outside_path = root.join("outside");
        std::fs::create_dir_all(&outside_path).unwrap();
        std::fs::write(outside_path.join("a.txt"), "a\n").unwrap();
        std::fs::write(outside_path.join("b.txt"), "other\n").unwrap();
        std::os::unix::fs::symlink(&outside_path, watched_path.join("thing")).unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::remove_file(watched_path.join("thing")).unwrap();
        std::fs::create_dir_all(watched_path.join("thing")).unwrap();
        std::fs::write(watched_path.join("thing/a.txt"), "a\n").unwrap();
        std::fs::write(watched_path.join("thing/b.txt"), "b\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        let v1_commit = watched.find_commit(v1).unwrap();
        watched
            .reset(v1_commit.as_object(), git2::ResetType::Mixed, None)
            .unwrap();
        drop(v1_commit);
        drop(watched);
        std::fs::remove_dir_all(watched_path.join("thing")).unwrap();
        std::os::unix::fs::symlink(&outside_path, watched_path.join("thing")).unwrap();

        processor_for(&watched_path)._remove_partial_update_files(v1, v2);

        assert_eq!(
            std::fs::read_to_string(outside_path.join("a.txt")).unwrap(),
            "a\n"
        );
        assert_eq!(
            std::fs::read_to_string(outside_path.join("b.txt")).unwrap(),
            "other\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_scoped_fallback_refuses_a_tree_whose_walk_fails() {
        let root = temp_dir("rollback-fallback-walk-fails");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        let odb = watched.odb().unwrap();
        let blob = odb.write(git2::ObjectType::Blob, b"x\n").unwrap();
        drop(odb);
        let bogus_tree = Oid::from_str("1111111111111111111111111111111111111111").unwrap();
        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(b"100644 aaa.txt\0");
        raw.extend_from_slice(blob.as_bytes());
        raw.extend_from_slice(b"40000 dir\0");
        raw.extend_from_slice(bogus_tree.as_bytes());
        raw.extend_from_slice(b"100644 file.txt\0");
        raw.extend_from_slice(blob.as_bytes());
        let odb = watched.odb().unwrap();
        let tree_oid = odb.write(git2::ObjectType::Tree, &raw).unwrap();
        drop(odb);
        let tree = watched.find_tree(tree_oid).unwrap();
        let sig = git2::Signature::now("zlorb-test", "zlorb-test@example.com").unwrap();
        let v1 = watched
            .commit(Some("HEAD"), &sig, &sig, "v1", &tree, &[])
            .unwrap();
        drop(tree);
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let processor = processor_for(&watched_path);
        assert!(processor._rollback_scoped_fallback_paths(v1, v2).is_none());
        assert!(processor._rollback_scoped_fallback_paths(v2, v1).is_none());
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_scoped_fallback_includes_fetch_only_paths() {
        let root = temp_dir("rollback-fallback-fetch-only");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        std::fs::write(watched_path.join("new.txt"), "new in v2\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let processor = processor_for(&watched_path);
        let (safe, preserved) = processor
            ._rollback_scoped_fallback_paths(v1, v2)
            .unwrap();
        assert!(safe.contains(&PathBuf::from("new.txt")));
        assert!(safe.contains(&PathBuf::from("src/main.rs")));
        assert!(preserved.is_empty());
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_scoped_fallback_preserves_a_path_matching_neither_tree() {
        let root = temp_dir("rollback-fallback-neither-tree");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        std::fs::write(watched_path.join("src/main.rs"), "user edit\n").unwrap();

        let processor = processor_for(&watched_path);
        let (safe, preserved) = processor
            ._rollback_scoped_fallback_paths(v1, v2)
            .unwrap();
        assert!(preserved.contains(&PathBuf::from("src/main.rs")));
        assert!(!safe.contains(&PathBuf::from("src/main.rs")));
        assert!(safe.contains(&PathBuf::from("Cargo.toml")));
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_scoped_fallback_preserves_an_edited_path_identical_in_both_trees() {
        let root = temp_dir("rollback-fallback-identical-edited");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::write(watched_path.join("unchanged.txt"), "unchanged\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        std::fs::write(watched_path.join("unchanged.txt"), "user edit\n").unwrap();

        let processor = processor_for(&watched_path);
        let (safe, preserved) = processor
            ._rollback_scoped_fallback_paths(v1, v2)
            .unwrap();
        assert!(preserved.contains(&PathBuf::from("unchanged.txt")));
        assert!(!safe.contains(&PathBuf::from("unchanged.txt")));
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_scoped_fallback_preserves_a_diverged_gitlink() {
        let (root, watched_path, v1, v2, _sub_a, _sub_b) =
            gitlink_fixture("rollback-fallback-gitlink-diverged");
        let sub_path = watched_path.join("sub");
        let sub = git2::Repository::open(&sub_path).unwrap();
        std::fs::write(sub_path.join("sub.txt"), "sub three\n").unwrap();
        commit_all(&sub, "sub c");
        drop(sub);

        let processor = processor_for(&watched_path);
        let (safe, preserved) = processor
            ._rollback_scoped_fallback_paths(v1, v2)
            .unwrap();
        assert!(preserved.contains(&PathBuf::from("sub")));
        assert!(!safe.contains(&PathBuf::from("sub")));
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_scoped_fallback_classifies_missing_paths_by_tree_membership() {
        let root = temp_dir("rollback-fallback-missing");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::write(watched_path.join("unchanged.txt"), "unchanged\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        std::fs::write(watched_path.join("new.txt"), "new in v2\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        std::fs::remove_file(watched_path.join("unchanged.txt")).unwrap();
        std::fs::remove_file(watched_path.join("new.txt")).unwrap();

        let processor = processor_for(&watched_path);
        let (safe, preserved) = processor
            ._rollback_scoped_fallback_paths(v1, v2)
            .unwrap();
        assert!(preserved.contains(&PathBuf::from("unchanged.txt")));
        assert!(!safe.contains(&PathBuf::from("unchanged.txt")));
        assert!(safe.contains(&PathBuf::from("new.txt")));
        assert!(!preserved.contains(&PathBuf::from("new.txt")));
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_scoped_fallback_preserves_a_retargeted_symlink_identical_in_both_trees() {
        let root = temp_dir("rollback-fallback-symlink-retargeted");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::os::unix::fs::symlink("src/main.rs", watched_path.join("link.txt")).unwrap();
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);
        std::fs::remove_file(watched_path.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("Cargo.toml", watched_path.join("link.txt")).unwrap();

        let processor = processor_for(&watched_path);
        let (safe, preserved) = processor
            ._rollback_scoped_fallback_paths(v1, v2)
            .unwrap();
        assert!(preserved.contains(&PathBuf::from("link.txt")));
        assert!(!safe.contains(&PathBuf::from("link.txt")));
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_fallback_refuses_when_the_rolled_back_tree_is_empty() {
        let root = temp_dir("rollback-fallback-empty-tree");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        let v1 = commit_all(&watched, "v1");
        std::fs::write(watched_path.join("file.txt"), "file\n").unwrap();
        let _v2 = commit_all(&watched, "v2");
        drop(watched);
        let bogus = Oid::from_str("1111111111111111111111111111111111111111").unwrap();

        let processor = processor_for(&watched_path);
        assert!(!processor._rollback_fast_forward(v1, bogus));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("file.txt")).unwrap(),
            "file\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_verification_reports_restored_and_unrestored_head() {
        let root = temp_dir("rollback-verification");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let processor = processor_for(&watched_path);
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-one")
        );

        let v2_commit = processor.repo.find_commit(v2).unwrap();
        processor
            .repo
            .reset(v2_commit.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(v2_commit);
        assert_eq!(head_oid(&processor.repo), v2);

        let lock_path = watched_path.join(".git/refs/heads/master.lock");
        std::fs::write(&lock_path, "").unwrap();
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v2);

        std::fs::remove_file(&lock_path).unwrap();
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_verification_follows_branch_ref_not_detached_head() {
        let root = temp_dir("rollback-verification-branch-ref");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let processor = processor_for(&watched_path);
        let branch_target = |processor: &RepoProcessor| {
            processor
                .repo
                .find_reference("refs/heads/master")
                .unwrap()
                .target()
                .unwrap()
        };

        processor.repo.set_head_detached(v1).unwrap();
        let lock_path = watched_path.join(".git/refs/heads/master.lock");
        std::fs::write(&lock_path, "").unwrap();
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(branch_target(&processor), v2);
        assert_eq!(head_oid(&processor.repo), v1);
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-one")
        );

        std::fs::remove_file(&lock_path).unwrap();
        processor.repo.set_head_detached(v2).unwrap();
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(branch_target(&processor), v1);
        assert_eq!(head_oid(&processor.repo), v1);
        assert!(!processor.repo.head_detached().unwrap());
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-one")
        );
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_restores_rolled_back_tree_when_head_detached_at_fetched_commit() {
        let root = temp_dir("rollback-tree-detached-at-fetched");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::write(watched_path.join("only-v1.txt"), "only in v1\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        std::fs::remove_file(watched_path.join("only-v1.txt")).unwrap();
        std::fs::write(watched_path.join("new.txt"), "new in v2\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let processor = processor_for(&watched_path);
        processor.repo.set_head_detached(v2).unwrap();
        let lock_path = watched_path.join(".git/refs/heads/master.lock");
        std::fs::write(&lock_path, "").unwrap();
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(
            processor
                .repo
                .find_reference("refs/heads/master")
                .unwrap()
                .target()
                .unwrap(),
            v2
        );
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-one")
        );
        assert!(!watched_path.join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(watched_path.join("only-v1.txt")).unwrap(),
            "only in v1\n"
        );
        assert!(processor.repo.head_detached().unwrap());
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_reports_failure_when_the_tree_cannot_be_restored() {
        let root = temp_dir("rollback-tree-restore-failure");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        std::fs::write(watched_path.join("only-v1.txt"), "only in v1\n").unwrap();
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        std::fs::remove_file(watched_path.join("only-v1.txt")).unwrap();
        std::fs::write(watched_path.join("new.txt"), "new in v2\n").unwrap();
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let processor = processor_for(&watched_path);
        let index_lock = watched_path.join(".git/index.lock");
        std::fs::write(&index_lock, "").unwrap();
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(
            processor
                .repo
                .find_reference("refs/heads/master")
                .unwrap()
                .target()
                .unwrap(),
            v1
        );

        std::fs::remove_file(&index_lock).unwrap();
        assert!(processor._rollback_fast_forward(v1, v2));
        assert!(
            std::fs::read_to_string(watched_path.join("src/main.rs"))
                .unwrap()
                .contains("fixture-version-one")
        );
        assert!(!watched_path.join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(watched_path.join("only-v1.txt")).unwrap(),
            "only in v1\n"
        );
        let mut status_opts = git2::StatusOptions::new();
        status_opts.include_untracked(false);
        let tracked_status_clean = {
            let statuses = processor
                .repo
                .statuses(Some(&mut status_opts))
                .unwrap();
            statuses.is_empty()
        };
        assert!(tracked_status_clean);
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_reports_failure_when_head_cannot_be_reattached() {
        let root = temp_dir("rollback-head-restore-failure");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let processor = processor_for(&watched_path);
        processor.repo.set_head_detached(v2).unwrap();
        let head_lock = watched_path.join(".git/HEAD.lock");
        std::fs::write(&head_lock, "").unwrap();
        assert!(!processor._rollback_fast_forward(v1, v2));
        assert_eq!(
            processor
                .repo
                .find_reference("refs/heads/master")
                .unwrap()
                .target()
                .unwrap(),
            v1
        );

        std::fs::remove_file(&head_lock).unwrap();
        assert!(processor._rollback_fast_forward(v1, v2));
        assert_eq!(head_oid(&processor.repo), v1);
        assert!(!processor.repo.head_detached().unwrap());
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_verification_fails_when_branch_ref_cannot_be_read() {
        let root = temp_dir("rollback-verification-unreadable-ref");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let watched = git2::Repository::init_opts(&watched_path, &init_opts).unwrap();
        write_fixture_crate(&watched_path, "fixture-version-one");
        let v1 = commit_all(&watched, "v1");
        write_fixture_crate(&watched_path, "fixture-version-two");
        let v2 = commit_all(&watched, "v2");
        drop(watched);

        let processor = processor_for(&watched_path);
        let ref_path = watched_path.join(".git/refs/heads/master");
        assert!(ref_path.exists());
        std::fs::remove_file(&ref_path).unwrap();
        assert!(!processor._verify_rollback(v1, v2));

        std::fs::write(&ref_path, "ref: refs/heads/master-missing\n").unwrap();
        assert!(!processor._verify_rollback(v1, v2));
        drop(processor);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_build_is_retried_on_later_cycles_until_it_succeeds() {
        let root = temp_dir("failed-build-retry");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        let v1 = commit_all(&upstream, "v1");
        write_sentinel_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);

        let failed_builds = new_failed_builds();
        std::fs::write(watched_path.join("fail-build"), "").unwrap();

        let err = processor_with_tracker(&watched_path, failed_builds.clone())
            .update_from_remote()
            .unwrap_err();
        assert!(format!("{:?}", err).contains("Failed to build"));
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert!(!fixture_artifact(&watched_path).exists());

        let err = processor_with_tracker(&watched_path, failed_builds.clone())
            .update_from_remote()
            .unwrap_err();
        assert!(format!("{:?}", err).contains("Failed to build"));
        assert!(!fixture_artifact(&watched_path).exists());

        std::fs::write(watched_path.join("src/main.rs"), "local edit\n").unwrap();
        let err = processor_with_tracker(&watched_path, failed_builds.clone())
            .update_from_remote()
            .unwrap_err();
        assert!(format!("{:?}", err).contains("local changes to tracked files"));
        assert!(!fixture_artifact(&watched_path).exists());
        write_sentinel_fixture_crate(&watched_path, "fixture-version-two");

        std::fs::remove_file(watched_path.join("fail-build")).unwrap();
        processor_with_tracker(&watched_path, failed_builds.clone())
            .update_from_remote()
            .unwrap();
        let artifact = fixture_artifact(&watched_path);
        assert!(artifact.exists());
        let output = std::process::Command::new(&artifact).output().unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("fixture-version-two")
        );

        std::fs::write(watched_path.join("fail-build"), "").unwrap();
        processor_with_tracker(&watched_path, failed_builds.clone())
            .update_from_remote()
            .unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn seeded_failure_for_a_bun_repo_is_retried_and_kept_when_bun_is_missing() {
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
        let bun_path = if user == "root" {
            PathBuf::from("/root/.bun/bin/bun")
        } else {
            PathBuf::from("/home").join(&user).join(".bun/bin/bun")
        };
        if bun_path.exists() {
            return;
        }
        let root = temp_dir("bun-retry");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        let v1 = commit_all(&upstream, "v1");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();

        let failed_builds = new_failed_builds();
        failed_builds
            .lock()
            .unwrap()
            .insert(tracker_key(&watched_path.to_string_lossy()), v1);
        let mut processor = processor_with_strategy(&watched_path, Some("bun"));
        processor.failed_builds = failed_builds.clone();
        let err = processor.update_from_remote().unwrap_err();
        assert!(format!("{:?}", err).contains("Bun is not installed"));
        assert_eq!(
            failed_builds
                .lock()
                .unwrap()
                .get(&tracker_key(&watched_path.to_string_lossy())),
            Some(&v1)
        );
        let mut processor = processor_with_strategy(&watched_path, Some("bun"));
        processor.failed_builds = failed_builds.clone();
        assert!(processor.update_from_remote().is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_build_record_is_cleared_when_a_newer_commit_builds() {
        let root = temp_dir("failed-build-cleared-by-newer");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        let v1 = commit_all(&upstream, "v1");
        write_sentinel_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);

        let failed_builds = new_failed_builds();
        std::fs::write(watched_path.join("fail-build"), "").unwrap();
        assert!(
            processor_with_tracker(&watched_path, failed_builds.clone())
                .update_from_remote()
                .is_err()
        );
        assert_eq!(
            failed_builds
                .lock()
                .unwrap()
                .get(&tracker_key(&watched_path.to_string_lossy())),
            Some(&v2)
        );

        let upstream = git2::Repository::open(&upstream_path).unwrap();
        write_sentinel_fixture_crate(&upstream_path, "fixture-version-three");
        let v3 = commit_all(&upstream, "v3");
        drop(upstream);
        std::fs::remove_file(watched_path.join("fail-build")).unwrap();
        processor_with_tracker(&watched_path, failed_builds.clone())
            .update_from_remote()
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v3);
        drop(watched);
        assert!(
            !failed_builds
                .lock()
                .unwrap()
                .contains_key(&tracker_key(&watched_path.to_string_lossy())),
            "a successful build at a newer commit must clear the failure record stored for the older commit"
        );
        let artifact = fixture_artifact(&watched_path);
        let output = std::process::Command::new(&artifact).output().unwrap();
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("fixture-version-three")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn processor_with_path_str(
        path: String,
        failed_builds: FailedBuilds,
        failed_restarts: FailedRestarts,
    ) -> RepoProcessor {
        let mut processor = RepoProcessor::new(RepositoryConfiguration {
            name: "watched".to_string(),
            path,
            remote: "origin".to_string(),
            branch: "master".to_string(),
            build_strategy: Some("rust".to_string()),
            build_command: String::new(),
            restart_command: None,
            out_dir: None,
        })
        .unwrap();
        processor.failed_builds = failed_builds;
        processor.failed_restarts = failed_restarts;
        processor
    }

    #[test]
    fn tracker_key_collapses_spelling_variants_of_the_same_repo() {
        let root = temp_dir("tracker-key");
        let repo_path = root.join("repo");
        std::fs::create_dir_all(repo_path.join("sub")).unwrap();
        let real = repo_path.to_string_lossy().into_owned();
        let key = tracker_key(&real);
        assert_eq!(tracker_key(&format!("{}/", real)), key);
        assert_eq!(tracker_key(&format!("{}/./", real)), key);
        assert_eq!(
            tracker_key(&repo_path.join("sub").join("..").to_string_lossy()),
            key
        );
        assert_eq!(tracker_key("/nope/a/./b/../c"), "/nope/a/c");
        assert_eq!(tracker_key("a/./b/../c"), "a/c");
        assert_eq!(tracker_key("/../a"), "/a");
        assert_eq!(normalize_path_lexically("../../a"), "../../a");
        assert_eq!(normalize_path_lexically("../.."), "../..");
        assert_eq!(normalize_path_lexically("x/../../../y"), "../../y");
        assert_eq!(normalize_path_lexically("../a/../.."), "../..");
        assert_eq!(normalize_path_lexically("a/../../b"), "../b");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tracker_key_resolves_symlinks_to_the_same_repo() {
        let root = temp_dir("tracker-key-symlink");
        let repo_path = root.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let link_path = root.join("link");
        std::os::unix::fs::symlink(&repo_path, &link_path).unwrap();
        assert_eq!(
            tracker_key(&link_path.to_string_lossy()),
            tracker_key(&repo_path.to_string_lossy())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_trackers_survive_a_path_spelling_edit() {
        let root = temp_dir("tracker-spelling");
        let repo_path = root.join("repo");
        git2::Repository::init(&repo_path).unwrap();
        let real = repo_path.to_string_lossy().into_owned();
        let failed_builds = new_failed_builds();
        let failed_restarts = new_failed_restarts();
        let oid = Oid::from_str("1111111111111111111111111111111111111111").unwrap();

        let plain = processor_with_path_str(
            real.clone(),
            failed_builds.clone(),
            failed_restarts.clone(),
        );
        plain.record_failed_build(oid);
        plain.record_failed_restart(oid);

        let respelled = processor_with_path_str(
            format!("{}/./", real),
            failed_builds.clone(),
            failed_restarts.clone(),
        );
        assert!(respelled.build_failed_at(oid));
        assert!(respelled.restart_failed_at(oid));

        let trailing = processor_with_path_str(
            format!("{}/", real),
            failed_builds.clone(),
            failed_restarts.clone(),
        );
        trailing.clear_failed_build();
        trailing.clear_failed_restart();
        assert!(!plain.build_failed_at(oid));
        assert!(!plain.restart_failed_at(oid));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_trackers_are_shared_across_a_symlinked_spelling() {
        let root = temp_dir("tracker-symlink-processors");
        let repo_path = root.join("repo");
        git2::Repository::init(&repo_path).unwrap();
        let link_path = root.join("link");
        std::os::unix::fs::symlink(&repo_path, &link_path).unwrap();
        let failed_builds = new_failed_builds();
        let failed_restarts = new_failed_restarts();
        let oid = Oid::from_str("3333333333333333333333333333333333333333").unwrap();

        let via_link = processor_with_path_str(
            link_path.to_string_lossy().into_owned(),
            failed_builds.clone(),
            failed_restarts.clone(),
        );
        via_link.record_failed_build(oid);
        via_link.record_failed_restart(oid);

        let via_real = processor_with_path_str(
            repo_path.to_string_lossy().into_owned(),
            failed_builds.clone(),
            failed_restarts.clone(),
        );
        assert!(via_real.build_failed_at(oid));
        assert!(via_real.restart_failed_at(oid));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tracker_key_is_stored_at_construction_not_recomputed_per_operation() {
        let root = temp_dir("tracker-stored-key");
        let repo_path = root.join("repo");
        git2::Repository::init(&repo_path).unwrap();
        let link_path = root.join("link");
        std::os::unix::fs::symlink(&repo_path, &link_path).unwrap();
        let failed_builds = new_failed_builds();
        let failed_restarts = new_failed_restarts();
        let oid = Oid::from_str("4444444444444444444444444444444444444444").unwrap();

        let processor = processor_with_path_str(
            link_path.to_string_lossy().into_owned(),
            failed_builds.clone(),
            failed_restarts.clone(),
        );
        std::fs::remove_file(&link_path).unwrap();
        processor.record_failed_build(oid);
        assert_eq!(
            failed_builds
                .lock()
                .unwrap()
                .get(&tracker_key(&repo_path.to_string_lossy())),
            Some(&oid),
            "the record must land under the canonical key captured at construction, not a per-operation fallback spelling"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn poisoned_trackers_degrade_without_panicking() {
        let root = temp_dir("tracker-poisoned");
        let repo_path = root.join("repo");
        git2::Repository::init(&repo_path).unwrap();
        let failed_builds = new_failed_builds();
        let failed_restarts = new_failed_restarts();
        for tracker in [&failed_builds, &failed_restarts] {
            let cloned = tracker.clone();
            let handle = std::thread::spawn(move || {
                let _guard = cloned.lock().unwrap();
                panic!("poison the tracker lock");
            });
            assert!(handle.join().is_err());
            assert!(tracker.lock().is_err());
        }
        let processor = processor_with_path_str(
            repo_path.to_string_lossy().into_owned(),
            failed_builds.clone(),
            failed_restarts.clone(),
        );
        let oid = Oid::from_str("2222222222222222222222222222222222222222").unwrap();
        assert!(!processor.build_failed_at(oid));
        assert!(!processor.restart_failed_at(oid));
        processor.record_failed_build(oid);
        processor.record_failed_restart(oid);
        processor.clear_failed_build();
        processor.clear_failed_restart();
        assert!(
            failed_builds
                .lock()
                .unwrap_err()
                .into_inner()
                .get(&tracker_key(&repo_path.to_string_lossy()))
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn processor_with_restart(
        watched: &Path,
        restart_command: Option<String>,
        failed_restarts: FailedRestarts,
    ) -> RepoProcessor {
        let mut processor = processor_for(watched);
        processor.config.restart_command = restart_command;
        processor.failed_restarts = failed_restarts;
        processor
    }

    fn serve_command(watched: &Path) -> String {
        format!(
            "\"{}\" > served.txt",
            fixture_artifact(watched).display()
        )
    }

    #[test]
    fn restart_command_succeeds_and_fails_with_its_stderr() {
        let dir = temp_dir("restart-unit");
        assert!(run_restart_command("true", &dir, "demo").is_ok());
        std::fs::write(dir.join("marker.txt"), "x").unwrap();
        assert!(run_restart_command("test -f marker.txt", &dir, "demo").is_ok());
        let err = run_restart_command("echo restart-boom >&2; exit 3", &dir, "demo")
            .unwrap_err();
        assert!(format!("{:?}", err).contains("Restart command for demo"));
        assert!(format!("{:?}", err).contains("restart-boom"));
        run_restart_command("echo ran > restart-ran.txt", &dir, "demo").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("restart-ran.txt")).unwrap(),
            "ran\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_command_is_not_blocked_by_a_backgrounded_process() {
        let dir = temp_dir("restart-background");
        let started = std::time::Instant::now();
        run_restart_command("sleep 20 & echo started", &dir, "demo").unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "restart command blocked on a backgrounded process for {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_runs_after_an_update_and_not_when_nothing_changes() {
        let root = temp_dir("restart-after-update");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        let v1 = commit_all(&upstream, "v1");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();

        let failed_restarts = new_failed_restarts();
        let command = serve_command(&watched_path);
        processor_with_restart(
            &watched_path,
            Some(command.clone()),
            failed_restarts.clone(),
        )
        .update_from_remote()
        .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v1);
        drop(watched);
        assert!(!watched_path.join("served.txt").exists());

        let upstream = git2::Repository::open(&upstream_path).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        processor_with_restart(
            &watched_path,
            Some(command.clone()),
            failed_restarts.clone(),
        )
        .update_from_remote()
        .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert_eq!(
            std::fs::read_to_string(watched_path.join("served.txt")).unwrap(),
            "fixture-version-two\n"
        );

        std::fs::remove_file(watched_path.join("served.txt")).unwrap();
        processor_with_restart(
            &watched_path,
            Some(command),
            failed_restarts.clone(),
        )
        .update_from_remote()
        .unwrap();
        assert!(!watched_path.join("served.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_restart_is_reported_and_retried_until_it_succeeds() {
        let root = temp_dir("restart-retry");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        commit_all(&upstream, "v1");
        write_fixture_crate(&upstream_path, "fixture-version-two");
        let v2 = commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(
            watched
                .revparse_single("HEAD~1")
                .unwrap()
                .id(),
        )
        .unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);

        let failed_restarts = new_failed_restarts();
        let err = processor_with_restart(
            &watched_path,
            Some("false".to_string()),
            failed_restarts.clone(),
        )
        .update_from_remote()
        .unwrap_err();
        assert!(format!("{:?}", err).contains("Restart command"));
        let watched = git2::Repository::open(&watched_path).unwrap();
        assert_eq!(head_oid(&watched), v2);
        drop(watched);
        assert_eq!(
            failed_restarts
                .lock()
                .unwrap()
                .get(&tracker_key(&watched_path.to_string_lossy())),
            Some(&v2)
        );

        let err = processor_with_restart(
            &watched_path,
            Some("false".to_string()),
            failed_restarts.clone(),
        )
        .update_from_remote()
        .unwrap_err();
        assert!(format!("{:?}", err).contains("Restart command"));

        std::fs::write(watched_path.join("src/main.rs"), "local edit\n").unwrap();
        let err = processor_with_restart(
            &watched_path,
            Some(serve_command(&watched_path)),
            failed_restarts.clone(),
        )
        .update_from_remote()
        .unwrap_err();
        assert!(
            format!("{:?}", err).contains("Refusing to run the restart command")
        );
        assert!(!watched_path.join("served.txt").exists());
        write_fixture_crate(&watched_path, "fixture-version-two");

        processor_with_restart(
            &watched_path,
            Some(serve_command(&watched_path)),
            failed_restarts.clone(),
        )
        .update_from_remote()
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(watched_path.join("served.txt")).unwrap(),
            "fixture-version-two\n"
        );
        assert!(
            !failed_restarts
                .lock()
                .unwrap()
                .contains_key(&tracker_key(&watched_path.to_string_lossy()))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restart_does_not_run_for_an_out_dir_triggered_build() {
        let root = temp_dir("restart-skipped-for-out-dir-build");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        commit_all(&upstream, "v1");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();

        let failed_restarts = new_failed_restarts();
        let mut processor = processor_with_restart(
            &watched_path,
            Some(serve_command(&watched_path)),
            failed_restarts.clone(),
        );
        processor.config.out_dir = Some("dist-out".to_string());
        processor.update_from_remote().unwrap();
        assert!(fixture_artifact(&watched_path).exists());
        assert!(!watched_path.join("served.txt").exists());

        let mut processor = processor_with_restart(
            &watched_path,
            Some(serve_command(&watched_path)),
            failed_restarts.clone(),
        );
        processor.config.out_dir = Some("dist-out".to_string());
        processor.update_from_remote().unwrap();
        assert!(!watched_path.join("served.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restart_does_not_run_when_the_build_fails() {
        let root = temp_dir("restart-skipped-on-build-failure");
        let upstream_path = root.join("upstream");
        let watched_path = root.join("watched");
        let mut init_opts = RepositoryInitOptions::new();
        init_opts.initial_head("master");
        let upstream = git2::Repository::init_opts(&upstream_path, &init_opts).unwrap();
        write_fixture_crate(&upstream_path, "fixture-version-one");
        let v1 = commit_all(&upstream, "v1");
        write_sentinel_fixture_crate(&upstream_path, "fixture-version-two");
        commit_all(&upstream, "v2");
        drop(upstream);
        git2::Repository::clone(upstream_path.to_string_lossy().as_ref(), &watched_path)
            .unwrap();
        let watched = git2::Repository::open(&watched_path).unwrap();
        let checkout = watched.find_commit(v1).unwrap();
        watched
            .reset(checkout.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        drop(checkout);
        drop(watched);

        let failed_builds = new_failed_builds();
        let failed_restarts = new_failed_restarts();
        std::fs::write(watched_path.join("fail-build"), "").unwrap();
        let mut processor = processor_with_restart(
            &watched_path,
            Some(serve_command(&watched_path)),
            failed_restarts.clone(),
        );
        processor.failed_builds = failed_builds.clone();
        let err = processor.update_from_remote().unwrap_err();
        assert!(format!("{:?}", err).contains("Failed to build"));
        assert!(!watched_path.join("served.txt").exists());

        std::fs::remove_file(watched_path.join("fail-build")).unwrap();
        let mut processor = processor_with_restart(
            &watched_path,
            Some(serve_command(&watched_path)),
            failed_restarts.clone(),
        );
        processor.failed_builds = failed_builds.clone();
        processor.update_from_remote().unwrap();
        assert_eq!(
            std::fs::read_to_string(watched_path.join("served.txt")).unwrap(),
            "fixture-version-two\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
