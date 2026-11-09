use crate::{repo_processor::RepoProcessor, service_config::ServiceConfig};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use zlorb_lib::config::RepositoryConfiguration;
use zlorb_lib::log::Logger;
use zlorb_lib::{create_file_with_content, error::ZlorbError, try_get_zlorb_config_dir};

pub struct ConfigManager {
    config_dir: PathBuf,
    reported_strategy_errors: RefCell<HashSet<String>>,
    reported_skip_errors: RefCell<HashSet<String>>,
    reported_parse_errors: RefCell<HashSet<String>>,
}

impl ConfigManager {
    pub fn try_new() -> Result<Self, ZlorbError> {
        Ok(Self::with_config_dir(try_get_zlorb_config_dir()?))
    }

    pub fn with_config_dir(config_dir: PathBuf) -> Self {
        Self {
            config_dir,
            reported_strategy_errors: RefCell::new(HashSet::new()),
            reported_skip_errors: RefCell::new(HashSet::new()),
            reported_parse_errors: RefCell::new(HashSet::new()),
        }
    }

    fn unreported_skip_errors(&self, skips: &[String]) -> Vec<String> {
        let mut current = HashSet::new();
        let mut fresh = Vec::new();
        {
            let mut reported = self.reported_skip_errors.borrow_mut();
            for skip in skips {
                current.insert(skip.clone());
                if reported.insert(skip.clone()) {
                    fresh.push(skip.clone());
                }
            }
            reported.retain(|key| current.contains(key));
        }
        fresh
    }

    fn unreported_parse_errors(&self, skips: &[String]) -> Vec<String> {
        let mut current = HashSet::new();
        let mut fresh = Vec::new();
        {
            let mut reported = self.reported_parse_errors.borrow_mut();
            for skip in skips {
                current.insert(skip.clone());
                if reported.insert(skip.clone()) {
                    fresh.push(skip.clone());
                }
            }
            reported.retain(|key| current.contains(key));
        }
        fresh
    }

    fn unreported_strategy_errors(
        &self,
        repos: &[RepositoryConfiguration],
    ) -> Vec<(String, bool)> {
        let mut current = HashSet::new();
        let mut fresh = Vec::new();
        {
            let mut reported = self.reported_strategy_errors.borrow_mut();
            for repo in repos {
                if let Err(e) = repo.resolved_build_strategy() {
                    let key = format!("{}\0{:?}", repo.name, e);
                    current.insert(key.clone());
                    if reported.insert(key) {
                        let blank = repo
                            .build_strategy
                            .as_deref()
                            .map(str::trim)
                            .unwrap_or("")
                            .is_empty()
                            && repo.build_command.trim().is_empty();
                        fresh.push((
                            format!("Invalid build strategy for repo {}: {:?}", repo.name, e),
                            blank,
                        ));
                    }
                }
            }
            reported.retain(|key| current.contains(key));
        }
        fresh
    }

    fn service_config_path(&self) -> PathBuf {
        self.config_dir.join("service-config.toml")
    }

    fn repositories_path(&self) -> PathBuf {
        self.config_dir.join("repositories.toml")
    }

    pub fn initialize_default_config(&self) -> Result<String, ZlorbError> {
        let config_path = self.service_config_path();
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent).map_err(ZlorbError::Io)?;
        }
        let c = ServiceConfig::default();
        let f = toml::to_string(&c).map_err(ZlorbError::TomlSerializationError)?;
        create_file_with_content(config_path, &f)?;
        Ok(f)
    }

    pub fn load_service_config(&self) -> Result<ServiceConfig, ZlorbError> {
        let config_file_path = self.service_config_path();
        let contents = match std::fs::read_to_string(&config_file_path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ZlorbError::ConfigNotFound(config_file_path));
            }
            Err(e) => return Err(ZlorbError::Io(e)),
        };
        toml::from_str::<ServiceConfig>(&contents)
            .map_err(ZlorbError::TomlDeserializationError)
    }

    pub fn load_repo_configurations(
        &self,
    ) -> Result<Vec<RepositoryConfiguration>, ZlorbError> {
        let path = self.repositories_path();
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.unreported_parse_errors(&[]);
                return Ok(Vec::new());
            }
            Err(e) => return Err(ZlorbError::Io(e)),
        };
        if contents.trim().is_empty() {
            self.unreported_parse_errors(&[]);
            return Ok(Vec::new());
        }
        let document = match toml::from_str::<toml::Value>(&contents) {
            Ok(document) => document,
            Err(e) => return Err(ZlorbError::TomlDeserializationError(e)),
        };
        let entries = match document.get("repository") {
            None => {
                self.unreported_parse_errors(&[]);
                return Ok(Vec::new());
            }
            Some(toml::Value::Array(entries)) => entries.clone(),
            Some(single) => vec![single.clone()],
        };
        let mut configs = Vec::new();
        let mut skips = Vec::new();
        for (index, entry) in entries.into_iter().enumerate() {
            match entry.try_into::<RepositoryConfiguration>() {
                Ok(config) => configs.push(config),
                Err(e) => {
                    skips.push(format!(
                        "Skipping invalid repository entry #{} in {}: {}",
                        index + 1,
                        path.display(),
                        e.to_string().trim_end()
                    ));
                }
            }
        }
        for message in self.unreported_parse_errors(&skips) {
            Logger::error(message);
        }
        Ok(configs)
    }

    #[cfg(test)]
    pub fn load_all_repo_configs(&self) -> Result<Vec<RepoProcessor>, ZlorbError> {
        let credentials = match self.load_service_config() {
            Ok(config) => config.credentials(),
            Err(e) => {
                Logger::error(format!(
                    "Failed to load service config for fetch credentials, continuing without them: {:?}",
                    e
                ));
                None
            }
        };
        self.load_all_repo_configs_with_credentials(credentials)
    }

    pub fn load_all_repo_configs_with_credentials(
        &self,
        credentials: Option<(String, String)>,
    ) -> Result<Vec<RepoProcessor>, ZlorbError> {
        let repos = self.load_repo_configurations()?;
        for (message, blank) in self.unreported_strategy_errors(&repos) {
            if blank {
                Logger::warn(message);
            } else {
                Logger::error(message);
            }
        }
        let mut processors = Vec::new();
        let mut skips = Vec::new();
        for repo in repos {
            match RepoProcessor::new(repo.clone()) {
                Ok(mut processor) => {
                    processor.set_credentials(credentials.clone());
                    processors.push(processor);
                }
                Err(e) => {
                    skips.push(format!(
                        "Skipping repo {} at {}: {:?}",
                        repo.name, repo.path, e
                    ));
                }
            }
        }
        for message in self.unreported_skip_errors(&skips) {
            Logger::error(message);
        }
        Ok(processors)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "zlorb-config-test-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        git2::Repository::init(path).unwrap();
    }

    fn repo_entry(name: &str, path: &Path) -> String {
        format!(
            "[[repository]]\nname = \"{}\"\npath = \"{}\"\nremote = \"origin\"\nbranch = \"master\"\nbuild_command = \"cargo build\"\n",
            name,
            path.display()
        )
    }

    fn write_repos(config_dir: &Path, contents: &str) {
        std::fs::create_dir_all(config_dir).unwrap();
        std::fs::write(config_dir.join("repositories.toml"), contents).unwrap();
    }

    #[test]
    fn missing_repositories_file_is_empty_not_error() {
        let config_dir = temp_dir("missing");
        let manager = ConfigManager::with_config_dir(config_dir);
        let repos = manager.load_all_repo_configs().unwrap();
        assert!(repos.is_empty());
    }

    #[test]
    fn malformed_repositories_file_is_error() {
        let config_dir = temp_dir("malformed");
        write_repos(&config_dir, "this is not = valid = toml [[");
        let manager = ConfigManager::with_config_dir(config_dir);
        assert!(manager.load_all_repo_configs().is_err());
        assert!(manager.load_repo_configurations().is_err());
    }

    #[test]
    fn bad_repo_entry_is_skipped_and_good_repo_still_loads() {
        let root = temp_dir("skip-bad");
        let config_dir = root.join("config");
        let good = root.join("good");
        init_repo(&good);
        let bad_path = root.join("does-not-exist");
        let contents = format!(
            "{}{}",
            repo_entry("good", &good),
            repo_entry("bad", &bad_path)
        );
        write_repos(&config_dir, &contents);
        let manager = ConfigManager::with_config_dir(config_dir);
        let repos = manager.load_all_repo_configs().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].config.name, "good");
    }

    #[test]
    fn repo_with_invalid_remote_branch_is_skipped_and_good_repo_still_loads() {
        let root = temp_dir("skip-invalid-remote-branch");
        let config_dir = root.join("config");
        let good = root.join("good");
        let bad = root.join("bad");
        init_repo(&good);
        init_repo(&bad);
        let bad_entry = format!(
            "[[repository]]\nname = \"bad\"\npath = \"{}\"\nremote = \"origin\"\nbranch = \"master\"\nremote_branch = \"foo..bar\"\nbuild_command = \"cargo build\"\n",
            bad.display()
        );
        let contents = format!("{}{}", repo_entry("good", &good), bad_entry);
        write_repos(&config_dir, &contents);
        let manager = ConfigManager::with_config_dir(config_dir);
        let repos = manager.load_all_repo_configs().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].config.name, "good");
    }

    #[test]
    fn repo_with_invalid_branch_is_skipped_and_good_repo_still_loads() {
        let root = temp_dir("skip-invalid-branch");
        let config_dir = root.join("config");
        let good = root.join("good");
        let bad = root.join("bad");
        init_repo(&good);
        init_repo(&bad);
        let bad_entry = format!(
            "[[repository]]\nname = \"bad\"\npath = \"{}\"\nremote = \"origin\"\nbranch = \"foo..bar\"\nbuild_command = \"cargo build\"\n",
            bad.display()
        );
        let contents = format!("{}{}", repo_entry("good", &good), bad_entry);
        write_repos(&config_dir, &contents);
        let manager = ConfigManager::with_config_dir(config_dir);
        let repos = manager.load_all_repo_configs().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].config.name, "good");
    }

    #[test]
    fn repo_with_invalid_remote_is_skipped_and_good_repo_still_loads() {
        let root = temp_dir("skip-invalid-remote");
        let config_dir = root.join("config");
        let good = root.join("good");
        let bad = root.join("bad");
        init_repo(&good);
        init_repo(&bad);
        let bad_entry = format!(
            "[[repository]]\nname = \"bad\"\npath = \"{}\"\nremote = \"foo bar\"\nbranch = \"master\"\nbuild_command = \"cargo build\"\n",
            bad.display()
        );
        let contents = format!("{}{}", repo_entry("good", &good), bad_entry);
        write_repos(&config_dir, &contents);
        let manager = ConfigManager::with_config_dir(config_dir);
        let repos = manager.load_all_repo_configs().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].config.name, "good");
    }

    #[test]
    fn reload_picks_up_repos_added_after_first_load() {
        let root = temp_dir("reload-add");
        let config_dir = root.join("config");
        let first = root.join("first");
        let second = root.join("second");
        init_repo(&first);
        init_repo(&second);
        write_repos(&config_dir, &repo_entry("first", &first));
        let manager = ConfigManager::with_config_dir(config_dir.clone());
        assert_eq!(manager.load_all_repo_configs().unwrap().len(), 1);
        let contents = format!(
            "{}{}",
            repo_entry("first", &first),
            repo_entry("second", &second)
        );
        write_repos(&config_dir, &contents);
        let reloaded = manager.load_all_repo_configs().unwrap();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded[1].config.name, "second");
    }

    #[test]
    fn reload_picks_up_edited_repo_and_service_config() {
        let root = temp_dir("reload-edit");
        let config_dir = root.join("config");
        let repo_path = root.join("repo");
        init_repo(&repo_path);
        write_repos(&config_dir, &repo_entry("repo", &repo_path));
        std::fs::write(
            config_dir.join("service-config.toml"),
            "sleep_time = 42\nusername = \"u\"\ntoken = \"t\"\n",
        )
        .unwrap();
        let manager = ConfigManager::with_config_dir(config_dir.clone());
        assert_eq!(manager.load_service_config().unwrap().sleep_time, 42);
        let before = manager.load_all_repo_configs().unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].config.build_command, "cargo build");
        assert_eq!(before[0].config.build_strategy, None);
        let edited = repo_entry("repo", &repo_path).replace(
            "build_command = \"cargo build\"",
            "build_strategy = \"rust\"\nbuild_command = \"cargo build --release\"",
        );
        write_repos(&config_dir, &edited);
        std::fs::write(
            config_dir.join("service-config.toml"),
            "sleep_time = 7\nusername = \"u\"\ntoken = \"t\"\n",
        )
        .unwrap();
        let reloaded = manager.load_all_repo_configs().unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].config.build_command, "cargo build --release");
        assert_eq!(reloaded[0].config.build_strategy.as_deref(), Some("rust"));
        assert_eq!(
            reloaded[0].config.resolved_build_strategy().unwrap(),
            zlorb_lib::config::BuildStrategyKind::Rust
        );
        assert_eq!(manager.load_service_config().unwrap().sleep_time, 7);
    }

    #[test]
    fn processors_get_credentials_from_service_config() {
        let root = temp_dir("processor-credentials");
        let config_dir = root.join("config");
        let repo_path = root.join("repo");
        init_repo(&repo_path);
        write_repos(&config_dir, &repo_entry("repo", &repo_path));
        std::fs::write(
            config_dir.join("service-config.toml"),
            "sleep_time = 5\nusername = \"alice\"\ntoken = \"secret\"\n",
        )
        .unwrap();
        let manager = ConfigManager::with_config_dir(config_dir);
        let repos = manager.load_all_repo_configs().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(
            repos[0].credentials(),
            Some(&("alice".to_string(), "secret".to_string()))
        );
    }

    #[test]
    fn processors_have_no_credentials_when_service_config_has_placeholders() {
        let root = temp_dir("processor-no-credentials");
        let config_dir = root.join("config");
        let repo_path = root.join("repo");
        init_repo(&repo_path);
        write_repos(&config_dir, &repo_entry("repo", &repo_path));
        std::fs::write(
            config_dir.join("service-config.toml"),
            "sleep_time = 5\nusername = \"NO USERNAME PROVIDED\"\ntoken = \"NO TOKEN PROVIDED\"\n",
        )
        .unwrap();
        let manager = ConfigManager::with_config_dir(config_dir.clone());
        let repos = manager.load_all_repo_configs().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].credentials(), None);
        std::fs::remove_file(config_dir.join("service-config.toml")).unwrap();
        let repos = manager.load_all_repo_configs().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].credentials(), None);
    }

    #[test]
    fn explicit_credentials_survive_a_malformed_service_config_file() {
        let root = temp_dir("processor-explicit-credentials");
        let config_dir = root.join("config");
        let repo_path = root.join("repo");
        init_repo(&repo_path);
        write_repos(&config_dir, &repo_entry("repo", &repo_path));
        std::fs::write(
            config_dir.join("service-config.toml"),
            "sleep_time = \"nope\"\n",
        )
        .unwrap();
        let manager = ConfigManager::with_config_dir(config_dir);
        let repos = manager
            .load_all_repo_configs_with_credentials(Some((
                "alice".to_string(),
                "secret".to_string(),
            )))
            .unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(
            repos[0].credentials(),
            Some(&("alice".to_string(), "secret".to_string()))
        );
        let repos = manager.load_all_repo_configs().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].credentials(), None);
    }

    #[test]
    fn entry_missing_a_field_is_skipped_and_good_entry_still_loads() {
        let root = temp_dir("skip-invalid-entry");
        let config_dir = root.join("config");
        let good = root.join("good");
        init_repo(&good);
        let bad = root.join("bad");
        init_repo(&bad);
        let bad_entry = format!(
            "[[repository]]\nname = \"bad\"\npath = \"{}\"\nremote = \"origin\"\nbuild_command = \"cargo build\"\n",
            bad.display()
        );
        let contents = format!("{}{}", repo_entry("good", &good), bad_entry);
        write_repos(&config_dir, &contents);
        let manager = ConfigManager::with_config_dir(config_dir);
        let parsed = manager.load_repo_configurations().unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "good");
        let repos = manager.load_all_repo_configs().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].config.name, "good");
    }

    fn repo_config(
        name: &str,
        build_strategy: Option<&str>,
        build_command: &str,
    ) -> RepositoryConfiguration {
        RepositoryConfiguration {
            name: name.to_string(),
            path: "/tmp/demo".to_string(),
            remote: "origin".to_string(),
            branch: "master".to_string(),
            remote_branch: None,
            build_strategy: build_strategy.map(|s| s.to_string()),
            build_command: build_command.to_string(),
            restart_command: None,
            out_dir: None,
        }
    }

    #[test]
    fn strategy_error_is_reported_once_across_reloads() {
        let manager = ConfigManager::with_config_dir(temp_dir("dedupe-once"));
        let repos = vec![repo_config("demo", Some("webpack"), "")];
        assert_eq!(manager.unreported_strategy_errors(&repos).len(), 1);
        assert!(manager.unreported_strategy_errors(&repos).is_empty());
        assert!(manager.unreported_strategy_errors(&repos).is_empty());
    }

    #[test]
    fn changed_strategy_error_is_reported_again() {
        let manager = ConfigManager::with_config_dir(temp_dir("dedupe-changed"));
        let first = vec![repo_config("demo", Some("webpack"), "")];
        assert_eq!(manager.unreported_strategy_errors(&first).len(), 1);
        let second = vec![repo_config("demo", Some("parcel"), "")];
        let fresh = manager.unreported_strategy_errors(&second);
        assert_eq!(fresh.len(), 1);
        assert!(fresh[0].0.contains("parcel"));
    }

    #[test]
    fn fixed_then_broken_strategy_error_is_reported_again() {
        let manager = ConfigManager::with_config_dir(temp_dir("dedupe-fixed"));
        let broken = vec![repo_config("demo", Some("webpack"), "")];
        assert_eq!(manager.unreported_strategy_errors(&broken).len(), 1);
        let fixed = vec![repo_config("demo", Some("rust"), "")];
        assert!(manager.unreported_strategy_errors(&fixed).is_empty());
        assert_eq!(manager.unreported_strategy_errors(&broken).len(), 1);
    }

    #[test]
    fn removed_then_readded_strategy_error_is_reported_again() {
        let manager = ConfigManager::with_config_dir(temp_dir("dedupe-removed"));
        let broken = vec![repo_config("demo", Some("webpack"), "")];
        assert_eq!(manager.unreported_strategy_errors(&broken).len(), 1);
        assert!(manager.unreported_strategy_errors(&[]).is_empty());
        assert_eq!(manager.unreported_strategy_errors(&broken).len(), 1);
    }

    #[test]
    fn skip_error_is_reported_once_across_reloads() {
        let manager = ConfigManager::with_config_dir(temp_dir("dedupe-skip-once"));
        let skips = vec!["Skipping repo bad at /tmp/bad: broken".to_string()];
        assert_eq!(manager.unreported_skip_errors(&skips).len(), 1);
        assert!(manager.unreported_skip_errors(&skips).is_empty());
        assert!(manager.unreported_skip_errors(&skips).is_empty());
    }

    #[test]
    fn changed_skip_error_is_reported_again() {
        let manager = ConfigManager::with_config_dir(temp_dir("dedupe-skip-changed"));
        let first = vec!["Skipping repo bad at /tmp/bad: broken".to_string()];
        assert_eq!(manager.unreported_skip_errors(&first).len(), 1);
        let second = vec!["Skipping repo bad at /tmp/bad: worse".to_string()];
        let fresh = manager.unreported_skip_errors(&second);
        assert_eq!(fresh.len(), 1);
        assert!(fresh[0].contains("worse"));
    }

    #[test]
    fn fixed_then_broken_skip_error_is_reported_again() {
        let manager = ConfigManager::with_config_dir(temp_dir("dedupe-skip-fixed"));
        let broken = vec!["Skipping repo bad at /tmp/bad: broken".to_string()];
        assert_eq!(manager.unreported_skip_errors(&broken).len(), 1);
        assert!(manager.unreported_skip_errors(&[]).is_empty());
        assert_eq!(manager.unreported_skip_errors(&broken).len(), 1);
    }

    #[test]
    fn parse_error_is_reported_once_across_reloads() {
        let manager = ConfigManager::with_config_dir(temp_dir("dedupe-parse-once"));
        let skips = vec![
            "Skipping invalid repository entry in /tmp/repositories.toml: missing field `branch`"
                .to_string(),
        ];
        assert_eq!(manager.unreported_parse_errors(&skips).len(), 1);
        assert!(manager.unreported_parse_errors(&skips).is_empty());
        assert!(manager.unreported_parse_errors(&skips).is_empty());
    }

    #[test]
    fn changed_parse_error_is_reported_again() {
        let manager = ConfigManager::with_config_dir(temp_dir("dedupe-parse-changed"));
        let first = vec![
            "Skipping invalid repository entry in /tmp/repositories.toml: missing field `branch`"
                .to_string(),
        ];
        assert_eq!(manager.unreported_parse_errors(&first).len(), 1);
        let second = vec![
            "Skipping invalid repository entry in /tmp/repositories.toml: missing field `remote`"
                .to_string(),
        ];
        let fresh = manager.unreported_parse_errors(&second);
        assert_eq!(fresh.len(), 1);
        assert!(fresh[0].contains("remote"));
    }

    #[test]
    fn fixed_then_broken_parse_error_is_reported_again() {
        let manager = ConfigManager::with_config_dir(temp_dir("dedupe-parse-fixed"));
        let broken = vec![
            "Skipping invalid repository entry in /tmp/repositories.toml: missing field `branch`"
                .to_string(),
        ];
        assert_eq!(manager.unreported_parse_errors(&broken).len(), 1);
        assert!(manager.unreported_parse_errors(&[]).is_empty());
        assert_eq!(manager.unreported_parse_errors(&broken).len(), 1);
    }

    fn bad_entry_contents(good: &Path, bad: &Path) -> String {
        let bad_entry = format!(
            "[[repository]]\nname = \"bad\"\npath = \"{}\"\nremote = \"origin\"\nbuild_command = \"cargo build\"\n",
            bad.display()
        );
        format!("{}{}", repo_entry("good", good), bad_entry)
    }

    fn expected_parse_skip(config_dir: &Path) -> String {
        format!(
            "Skipping invalid repository entry #2 in {}: missing field `branch`",
            config_dir.join("repositories.toml").display()
        )
    }

    #[test]
    fn load_reports_parse_error_through_the_dedupe_set() {
        let root = temp_dir("parse-wiring-once");
        let config_dir = root.join("config");
        let good = root.join("good");
        let bad = root.join("bad");
        init_repo(&good);
        init_repo(&bad);
        write_repos(&config_dir, &bad_entry_contents(&good, &bad));
        let manager = ConfigManager::with_config_dir(config_dir.clone());
        let expected = expected_parse_skip(&config_dir);
        assert_eq!(manager.load_repo_configurations().unwrap().len(), 1);
        assert!(manager
            .unreported_parse_errors(&[expected.clone()])
            .is_empty());
        assert_eq!(manager.load_repo_configurations().unwrap().len(), 1);
        assert!(manager.unreported_parse_errors(&[expected]).is_empty());
    }

    #[test]
    fn fixed_entry_clears_the_reported_parse_error() {
        let root = temp_dir("parse-wiring-fixed");
        let config_dir = root.join("config");
        let good = root.join("good");
        let bad = root.join("bad");
        init_repo(&good);
        init_repo(&bad);
        write_repos(&config_dir, &bad_entry_contents(&good, &bad));
        let manager = ConfigManager::with_config_dir(config_dir.clone());
        let expected = expected_parse_skip(&config_dir);
        assert_eq!(manager.load_repo_configurations().unwrap().len(), 1);
        let fixed = format!(
            "{}{}",
            repo_entry("good", &good),
            repo_entry("bad", &bad)
        );
        write_repos(&config_dir, &fixed);
        assert_eq!(manager.load_repo_configurations().unwrap().len(), 2);
        assert_eq!(
            manager.unreported_parse_errors(&[expected]).len(),
            1,
            "a parse error that cleared must be reportable again"
        );
    }

    #[test]
    fn removed_repositories_file_clears_the_reported_parse_error() {
        let root = temp_dir("parse-wiring-removed");
        let config_dir = root.join("config");
        let good = root.join("good");
        let bad = root.join("bad");
        init_repo(&good);
        init_repo(&bad);
        write_repos(&config_dir, &bad_entry_contents(&good, &bad));
        let manager = ConfigManager::with_config_dir(config_dir.clone());
        let expected = expected_parse_skip(&config_dir);
        assert_eq!(manager.load_repo_configurations().unwrap().len(), 1);
        std::fs::remove_file(config_dir.join("repositories.toml")).unwrap();
        assert!(manager.load_repo_configurations().unwrap().is_empty());
        assert_eq!(manager.unreported_parse_errors(&[expected]).len(), 1);
    }

    #[test]
    fn empty_repositories_file_clears_the_reported_parse_error() {
        let root = temp_dir("parse-wiring-empty");
        let config_dir = root.join("config");
        let good = root.join("good");
        let bad = root.join("bad");
        init_repo(&good);
        init_repo(&bad);
        write_repos(&config_dir, &bad_entry_contents(&good, &bad));
        let manager = ConfigManager::with_config_dir(config_dir.clone());
        let expected = expected_parse_skip(&config_dir);
        assert_eq!(manager.load_repo_configurations().unwrap().len(), 1);
        write_repos(&config_dir, "");
        assert!(manager.load_repo_configurations().unwrap().is_empty());
        assert_eq!(manager.unreported_parse_errors(&[expected]).len(), 1);
    }

    #[test]
    fn repositories_file_without_repository_key_clears_the_reported_parse_error() {
        let root = temp_dir("parse-wiring-no-key");
        let config_dir = root.join("config");
        let good = root.join("good");
        let bad = root.join("bad");
        init_repo(&good);
        init_repo(&bad);
        write_repos(&config_dir, &bad_entry_contents(&good, &bad));
        let manager = ConfigManager::with_config_dir(config_dir.clone());
        let expected = expected_parse_skip(&config_dir);
        assert_eq!(manager.load_repo_configurations().unwrap().len(), 1);
        write_repos(&config_dir, "# no repos yet\n");
        assert!(manager.load_repo_configurations().unwrap().is_empty());
        assert_eq!(manager.unreported_parse_errors(&[expected]).len(), 1);
    }

    #[test]
    fn malformed_repositories_file_leaves_the_reported_parse_error_alone() {
        let root = temp_dir("parse-wiring-malformed");
        let config_dir = root.join("config");
        let good = root.join("good");
        let bad = root.join("bad");
        init_repo(&good);
        init_repo(&bad);
        write_repos(&config_dir, &bad_entry_contents(&good, &bad));
        let manager = ConfigManager::with_config_dir(config_dir.clone());
        let expected = expected_parse_skip(&config_dir);
        assert_eq!(manager.load_repo_configurations().unwrap().len(), 1);
        write_repos(&config_dir, "this is not = valid = toml [[");
        assert!(manager.load_repo_configurations().is_err());
        assert!(
            manager.unreported_parse_errors(&[expected]).is_empty(),
            "an errored load must not clear the reported set, matching the skip and strategy dedupes"
        );
    }


    #[test]
    fn blank_ctl_state_is_flagged_blank_and_unknown_strategy_is_not() {
        let manager = ConfigManager::with_config_dir(temp_dir("dedupe-blank"));
        let blank = vec![repo_config("fresh", None, "")];
        let fresh = manager.unreported_strategy_errors(&blank);
        assert_eq!(fresh.len(), 1);
        assert!(fresh[0].1);
        let unknown = vec![repo_config("demo", Some("webpack"), "")];
        let fresh = manager.unreported_strategy_errors(&unknown);
        assert_eq!(fresh.len(), 1);
        assert!(!fresh[0].1);
        let legacy = vec![repo_config("old", None, "bunny")];
        let fresh = manager.unreported_strategy_errors(&legacy);
        assert_eq!(fresh.len(), 1);
        assert!(!fresh[0].1);
    }

    #[test]
    fn valid_repos_have_no_strategy_errors_to_report() {
        let manager = ConfigManager::with_config_dir(temp_dir("dedupe-valid"));
        let repos = vec![
            repo_config("a", Some("bun"), ""),
            repo_config("b", None, "cargo build"),
        ];
        assert!(manager.unreported_strategy_errors(&repos).is_empty());
    }

    #[test]
    fn comment_only_repositories_file_is_empty_not_error() {
        let config_dir = temp_dir("comment-only");
        write_repos(&config_dir, "# no repos yet\n");
        let manager = ConfigManager::with_config_dir(config_dir);
        assert!(manager.load_all_repo_configs().unwrap().is_empty());
    }

    #[test]
    fn missing_service_config_is_error_not_rewritten() {
        let config_dir = temp_dir("missing-service");
        std::fs::create_dir_all(&config_dir).unwrap();
        let manager = ConfigManager::with_config_dir(config_dir.clone());
        assert!(manager.load_service_config().is_err());
        assert!(!config_dir.join("service-config.toml").exists());
    }

    #[test]
    fn malformed_service_config_is_error() {
        let config_dir = temp_dir("bad-service");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("service-config.toml"), "sleep_time = \"nope\"\n").unwrap();
        let manager = ConfigManager::with_config_dir(config_dir);
        assert!(manager.load_service_config().is_err());
    }
}
