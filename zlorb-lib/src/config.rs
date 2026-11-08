use serde::{Deserialize, Serialize};

use crate::error::ZlorbError;

macro_rules! build_strategy_kinds {
    ($($kind:ident => $name:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum BuildStrategyKind {
            $($kind),+
        }

        impl BuildStrategyKind {
            pub const ALL: &'static [Self] = &[$(Self::$kind),+];

            pub fn name(&self) -> &'static str {
                match self {
                    $(Self::$kind => $name),+
                }
            }
        }
    };
}

build_strategy_kinds!(Bun => "bun", Rust => "rust");

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfiguration {
    pub name: String,
    pub path: String,
    pub remote: String,
    pub branch: String,
    #[serde(default)]
    pub remote_branch: Option<String>,
    #[serde(default)]
    pub build_strategy: Option<String>,
    #[serde(default)]
    pub build_command: String,
    #[serde(default)]
    pub restart_command: Option<String>,
    pub out_dir: Option<String>,
}

impl RepositoryConfiguration {
    pub fn resolved_remote_branch(&self) -> &str {
        self.remote_branch
            .as_deref()
            .map(str::trim)
            .filter(|branch| !branch.is_empty())
            .unwrap_or(&self.branch)
    }

    pub fn validate_branch_names(&self) -> Result<(), ZlorbError> {
        if !is_usable_branch_name(&self.branch) {
            return Err(ZlorbError::InvalidConfig(format!(
                "repo '{}' has invalid branch '{}' in repositories.toml; set branch to a valid branch name",
                self.name, self.branch
            )));
        }
        let remote_branch = self.resolved_remote_branch();
        if !is_usable_branch_name(remote_branch) {
            return Err(ZlorbError::InvalidConfig(format!(
                "repo '{}' has invalid remote_branch '{}' in repositories.toml; set remote_branch to a valid branch name or remove it to track branch '{}'",
                self.name, remote_branch, self.branch
            )));
        }
        Ok(())
    }

    pub fn validate_remote_name(&self) -> Result<(), ZlorbError> {
        if !is_usable_remote_name(&self.remote) {
            return Err(ZlorbError::InvalidConfig(format!(
                "repo '{}' has invalid remote '{}' in repositories.toml; set remote to a valid remote name",
                self.name, self.remote
            )));
        }
        Ok(())
    }

    pub fn resolved_restart_command(&self) -> Option<&str> {
        self.restart_command
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
    }

    pub fn resolved_build_strategy(&self) -> Result<BuildStrategyKind, ZlorbError> {
        let names: Vec<String> = BuildStrategyKind::ALL
            .iter()
            .map(|kind| format!("\"{}\"", kind.name()))
            .collect();
        let accepted_owned = format!(
            "{}, or \"cargo\" (alias for \"{}\")",
            names.join(", "),
            BuildStrategyKind::Rust.name()
        );
        let accepted = accepted_owned.as_str();
        if let Some(raw) = &self.build_strategy {
            let value = raw.trim().to_lowercase();
            if !value.is_empty() {
                if let Some(kind) = BuildStrategyKind::ALL
                    .iter()
                    .find(|kind| kind.name() == value.as_str())
                {
                    return Ok(*kind);
                }
                return match value.as_str() {
                    "cargo" => Ok(BuildStrategyKind::Rust),
                    _ => Err(ZlorbError::InvalidConfig(format!(
                        "repo '{}' has unknown build_strategy '{}'; expected {}",
                        self.name, raw, accepted
                    ))),
                };
            }
        }
        let legacy = self.build_command.trim().to_lowercase();
        match legacy.as_str() {
            "bun" => Ok(BuildStrategyKind::Bun),
            "cargo" | "cargo build" => Ok(BuildStrategyKind::Rust),
            "" => Err(ZlorbError::InvalidConfig(format!(
                "repo '{}' has no build_strategy; set build_strategy to {} in repositories.toml",
                self.name, accepted
            ))),
            _ => Err(ZlorbError::InvalidConfig(format!(
                "repo '{}' has no build_strategy and its legacy build_command '{}' does not name a strategy; set build_strategy to {} in repositories.toml",
                self.name,
                self.build_command.trim(),
                accepted
            ))),
        }
    }
}

fn is_usable_remote_name(value: &str) -> bool {
    !value.is_empty() && !value.contains('\0') && git2::Remote::is_valid_name(value)
}

fn is_usable_branch_name(value: &str) -> bool {
    value != "HEAD"
        && !value.contains('\0')
        && !value.starts_with("refs/")
        && git2::Reference::is_valid_name(&format!("refs/heads/{}", value))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoriesConfigurationFile {
    #[serde(default, rename = "repository")]
    pub repositories: Vec<RepositoryConfiguration>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(build_strategy: Option<&str>, build_command: &str) -> RepositoryConfiguration {
        RepositoryConfiguration {
            name: "demo".to_string(),
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
    fn all_kinds_contains_each_variant_once() {
        assert_eq!(BuildStrategyKind::ALL.len(), 2);
        for kind in [BuildStrategyKind::Bun, BuildStrategyKind::Rust] {
            assert_eq!(
                BuildStrategyKind::ALL.iter().filter(|k| **k == kind).count(),
                1,
                "ALL must contain {kind:?} exactly once"
            );
        }
    }

    #[test]
    fn kind_names_match_their_serde_names() {
        for kind in BuildStrategyKind::ALL {
            let serialized = serde_json::to_string(kind).unwrap();
            assert_eq!(
                serialized.trim_matches('"'),
                kind.name(),
                "serde name and name() must agree for {kind:?}"
            );
        }
    }

    #[test]
    fn kind_names_resolve_back_to_their_kind() {
        for kind in BuildStrategyKind::ALL {
            assert_eq!(
                config(Some(kind.name()), "").resolved_build_strategy().unwrap(),
                *kind,
                "name() for {kind:?} must resolve back to it"
            );
        }
    }

    #[test]
    fn explicit_bun_selects_bun() {
        assert_eq!(
            config(Some("bun"), "").resolved_build_strategy().unwrap(),
            BuildStrategyKind::Bun
        );
    }

    #[test]
    fn explicit_rust_selects_rust() {
        assert_eq!(
            config(Some("rust"), "").resolved_build_strategy().unwrap(),
            BuildStrategyKind::Rust
        );
    }

    #[test]
    fn explicit_cargo_selects_rust() {
        assert_eq!(
            config(Some("cargo"), "").resolved_build_strategy().unwrap(),
            BuildStrategyKind::Rust
        );
    }

    #[test]
    fn explicit_strategy_wins_over_build_command() {
        assert_eq!(
            config(Some("bun"), "cargo build")
                .resolved_build_strategy()
                .unwrap(),
            BuildStrategyKind::Bun
        );
    }

    #[test]
    fn unknown_explicit_strategy_is_error() {
        let err = config(Some("webpack"), "")
            .resolved_build_strategy()
            .unwrap_err();
        assert!(format!("{:?}", err).contains("webpack"));
        assert!(format!("{:?}", err).contains("demo"));
    }

    #[test]
    fn strategy_error_messages_list_the_full_accepted_set() {
        for err in [
            config(Some("webpack"), "").resolved_build_strategy().unwrap_err(),
            config(None, "").resolved_build_strategy().unwrap_err(),
            config(None, "bunny").resolved_build_strategy().unwrap_err(),
        ] {
            let ZlorbError::InvalidConfig(message) = err else {
                panic!("expected InvalidConfig, got {:?}", err);
            };
            for kind in BuildStrategyKind::ALL {
                assert!(
                    message.contains(&format!("\"{}\"", kind.name())),
                    "missing {} in: {}",
                    kind.name(),
                    message
                );
            }
            assert!(
                message.contains("\"bun\", \"rust\", or \"cargo\" (alias for \"rust\")"),
                "accepted set wording changed in: {}",
                message
            );
            assert!(message.contains("\"cargo\""), "missing cargo in: {}", message);
            assert!(
                message.contains(&format!(
                    "alias for \"{}\"",
                    BuildStrategyKind::Rust.name()
                )),
                "cargo is not named as the rust alias in: {}",
                message
            );
        }
    }

    #[test]
    fn explicit_strategy_is_case_insensitive() {
        assert_eq!(
            config(Some(" BUN "), "").resolved_build_strategy().unwrap(),
            BuildStrategyKind::Bun
        );
        assert_eq!(
            config(Some("Cargo"), "").resolved_build_strategy().unwrap(),
            BuildStrategyKind::Rust
        );
    }

    #[test]
    fn legacy_commands_follow_the_same_case_rule_as_explicit_strategies() {
        assert_eq!(
            config(None, "BUN").resolved_build_strategy().unwrap(),
            BuildStrategyKind::Bun
        );
        assert_eq!(
            config(None, "Cargo").resolved_build_strategy().unwrap(),
            BuildStrategyKind::Rust
        );
        assert_eq!(
            config(None, "  Cargo Build  ").resolved_build_strategy().unwrap(),
            BuildStrategyKind::Rust
        );
    }

    #[test]
    fn legacy_bun_command_selects_bun() {
        assert_eq!(
            config(None, "bun").resolved_build_strategy().unwrap(),
            BuildStrategyKind::Bun
        );
    }

    #[test]
    fn legacy_cargo_commands_select_rust() {
        assert_eq!(
            config(None, "cargo").resolved_build_strategy().unwrap(),
            BuildStrategyKind::Rust
        );
        assert_eq!(
            config(None, "cargo build")
                .resolved_build_strategy()
                .unwrap(),
            BuildStrategyKind::Rust
        );
    }

    #[test]
    fn missing_strategy_and_command_is_error() {
        assert!(config(None, "").resolved_build_strategy().is_err());
        assert!(config(Some(""), "").resolved_build_strategy().is_err());
        assert!(config(Some("  "), "").resolved_build_strategy().is_err());
    }

    #[test]
    fn substring_lookalikes_do_not_select_a_strategy() {
        assert!(config(None, "bunny").resolved_build_strategy().is_err());
        assert!(config(None, "cargo build --release")
            .resolved_build_strategy()
            .is_err());
        assert!(config(None, "bun run build")
            .resolved_build_strategy()
            .is_err());
    }

    #[test]
    fn toml_without_build_strategy_uses_legacy_command() {
        let parsed: RepositoryConfiguration = toml::from_str(
            "name = \"demo\"\npath = \"/tmp/demo\"\nremote = \"origin\"\nbranch = \"master\"\nbuild_command = \"bun\"\n",
        )
        .unwrap();
        assert_eq!(parsed.build_strategy, None);
        assert_eq!(
            parsed.resolved_build_strategy().unwrap(),
            BuildStrategyKind::Bun
        );
    }

    #[test]
    fn toml_entry_missing_build_command_parses_via_value() {
        let document: toml::Value = toml::from_str(
            "[[repository]]\nname = \"demo\"\npath = \"/tmp/demo\"\nremote = \"origin\"\nbranch = \"master\"\nbuild_strategy = \"bun\"\n",
        )
        .unwrap();
        let entry = document
            .get("repository")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .clone();
        let parsed: RepositoryConfiguration = entry.try_into().unwrap();
        assert_eq!(parsed.build_command, "");
        assert_eq!(
            parsed.resolved_build_strategy().unwrap(),
            BuildStrategyKind::Bun
        );
    }

    #[test]
    fn toml_with_restart_command_parses_and_resolves_it() {
        let parsed: RepositoryConfiguration = toml::from_str(
            "name = \"demo\"\npath = \"/tmp/demo\"\nremote = \"origin\"\nbranch = \"master\"\nbuild_strategy = \"rust\"\nrestart_command = \" ./deploy.sh \"\n",
        )
        .unwrap();
        assert_eq!(parsed.restart_command.as_deref(), Some(" ./deploy.sh "));
        assert_eq!(parsed.resolved_restart_command(), Some("./deploy.sh"));
    }

    #[test]
    fn missing_or_blank_restart_command_resolves_to_none() {
        let parsed: RepositoryConfiguration = toml::from_str(
            "name = \"demo\"\npath = \"/tmp/demo\"\nremote = \"origin\"\nbranch = \"master\"\nbuild_strategy = \"rust\"\n",
        )
        .unwrap();
        assert_eq!(parsed.restart_command, None);
        assert_eq!(parsed.resolved_restart_command(), None);
        let mut blank = config(None, "");
        blank.restart_command = Some("  ".to_string());
        assert_eq!(blank.resolved_restart_command(), None);
    }

    #[test]
    fn remote_branch_defaults_to_the_local_branch() {
        let parsed: RepositoryConfiguration = toml::from_str(
            "name = \"demo\"\npath = \"/tmp/demo\"\nremote = \"origin\"\nbranch = \"dev\"\nbuild_strategy = \"rust\"\n",
        )
        .unwrap();
        assert_eq!(parsed.remote_branch, None);
        assert_eq!(parsed.resolved_remote_branch(), "dev");
        let mut blank = config(None, "");
        blank.remote_branch = Some("  ".to_string());
        assert_eq!(blank.resolved_remote_branch(), "master");
    }

    #[test]
    fn toml_with_remote_branch_parses_and_resolves_it() {
        let parsed: RepositoryConfiguration = toml::from_str(
            "name = \"demo\"\npath = \"/tmp/demo\"\nremote = \"upstream\"\nbranch = \"dev\"\nremote_branch = \"main\"\nbuild_strategy = \"rust\"\n",
        )
        .unwrap();
        assert_eq!(parsed.remote_branch.as_deref(), Some("main"));
        assert_eq!(parsed.resolved_remote_branch(), "main");
        assert_eq!(parsed.branch, "dev");
    }

    #[test]
    fn valid_branch_names_pass_validation() {
        assert!(config(None, "").validate_branch_names().is_ok());
        let mut nested = config(None, "");
        nested.branch = "feature/foo".to_string();
        nested.remote_branch = Some("release/1.2".to_string());
        assert!(nested.validate_branch_names().is_ok());
    }

    #[test]
    fn blank_remote_branch_is_validated_as_the_local_branch() {
        let mut blank = config(None, "");
        blank.remote_branch = Some("  ".to_string());
        assert!(blank.validate_branch_names().is_ok());
        blank.branch = "foo..bar".to_string();
        let err = blank.validate_branch_names().unwrap_err();
        assert!(
            format!("{:?}", err).contains("invalid branch 'foo..bar'"),
            "unexpected error: {:?}",
            err
        );
    }

    #[test]
    fn invalid_branch_fails_validation_naming_the_repo_and_value() {
        let mut bad = config(None, "");
        bad.branch = "foo..bar".to_string();
        let err = bad.validate_branch_names().unwrap_err();
        let ZlorbError::InvalidConfig(message) = err else {
            panic!("expected InvalidConfig, got {:?}", err);
        };
        assert!(message.contains("demo"), "missing repo name in: {}", message);
        assert!(
            message.contains("invalid branch 'foo..bar'"),
            "missing value in: {}",
            message
        );
        assert!(
            message.contains("repositories.toml"),
            "missing file in: {}",
            message
        );
    }

    #[test]
    fn invalid_remote_branch_fails_validation_naming_the_repo_and_value() {
        let mut bad = config(None, "");
        bad.remote_branch = Some("foo..bar".to_string());
        let err = bad.validate_branch_names().unwrap_err();
        let ZlorbError::InvalidConfig(message) = err else {
            panic!("expected InvalidConfig, got {:?}", err);
        };
        assert!(message.contains("demo"), "missing repo name in: {}", message);
        assert!(
            message.contains("invalid remote_branch 'foo..bar'"),
            "missing value in: {}",
            message
        );
        assert!(
            message.contains("remove it"),
            "missing fallback advice in: {}",
            message
        );
    }

    #[test]
    fn padded_invalid_remote_branch_is_validated_after_trimming() {
        let mut bad = config(None, "");
        bad.remote_branch = Some(" foo..bar ".to_string());
        let err = bad.validate_branch_names().unwrap_err();
        assert!(
            format!("{:?}", err).contains("invalid remote_branch 'foo..bar'"),
            "unexpected error: {:?}",
            err
        );
    }

    #[test]
    fn ref_prefixed_branch_names_fail_validation() {
        let mut bad = config(None, "");
        bad.branch = "refs/heads/main".to_string();
        let err = bad.validate_branch_names().unwrap_err();
        assert!(
            format!("{:?}", err).contains("invalid branch 'refs/heads/main'"),
            "unexpected error: {:?}",
            err
        );
        let mut bad_remote = config(None, "");
        bad_remote.remote_branch = Some("refs/heads/main".to_string());
        let err = bad_remote.validate_branch_names().unwrap_err();
        assert!(
            format!("{:?}", err).contains("invalid remote_branch 'refs/heads/main'"),
            "unexpected error: {:?}",
            err
        );
    }

    #[test]
    fn head_fails_validation_as_a_branch_name() {
        let mut bad = config(None, "");
        bad.branch = "HEAD".to_string();
        assert!(bad.validate_branch_names().is_err());
        let mut bad_remote = config(None, "");
        bad_remote.remote_branch = Some("HEAD".to_string());
        let err = bad_remote.validate_branch_names().unwrap_err();
        assert!(
            format!("{:?}", err).contains("invalid remote_branch 'HEAD'"),
            "unexpected error: {:?}",
            err
        );
    }

    #[test]
    fn branch_name_with_a_space_fails_validation() {
        let mut bad = config(None, "");
        bad.branch = "my branch".to_string();
        assert!(bad.validate_branch_names().is_err());
    }

    #[test]
    fn empty_branch_fails_validation() {
        let mut bad = config(None, "");
        bad.branch = String::new();
        assert!(bad.validate_branch_names().is_err());
    }

    #[test]
    fn valid_remote_names_pass_validation() {
        assert!(config(None, "").validate_remote_name().is_ok());
        let mut other = config(None, "");
        other.remote = "upstream".to_string();
        assert!(other.validate_remote_name().is_ok());
        other.remote = "my-remote".to_string();
        assert!(other.validate_remote_name().is_ok());
    }

    #[test]
    fn invalid_remote_fails_validation_naming_the_repo_and_value() {
        let mut bad = config(None, "");
        bad.remote = "foo bar".to_string();
        let err = bad.validate_remote_name().unwrap_err();
        let ZlorbError::InvalidConfig(message) = err else {
            panic!("expected InvalidConfig, got {:?}", err);
        };
        assert!(message.contains("demo"), "missing repo name in: {}", message);
        assert!(
            message.contains("invalid remote 'foo bar'"),
            "missing value in: {}",
            message
        );
        assert!(
            message.contains("repositories.toml"),
            "missing file in: {}",
            message
        );
    }

    #[test]
    fn empty_remote_fails_validation() {
        let mut bad = config(None, "");
        bad.remote = String::new();
        assert!(bad.validate_remote_name().is_err());
    }

    #[test]
    fn remote_with_double_dot_fails_validation() {
        let mut bad = config(None, "");
        bad.remote = "foo..bar".to_string();
        assert!(bad.validate_remote_name().is_err());
    }

    #[test]
    fn nul_in_remote_fails_validation_without_panicking() {
        let mut bad = config(None, "");
        bad.remote = "foo\0bar".to_string();
        assert!(bad.validate_remote_name().is_err());
    }

    #[test]
    fn nul_in_branch_fails_validation_without_panicking() {
        let mut bad = config(None, "");
        bad.branch = "foo\0bar".to_string();
        assert!(bad.validate_branch_names().is_err());
        let mut bad_remote = config(None, "");
        bad_remote.remote_branch = Some("foo\0bar".to_string());
        assert!(bad_remote.validate_branch_names().is_err());
    }

    #[test]
    fn toml_with_build_strategy_parses_it() {
        let parsed: RepositoryConfiguration = toml::from_str(
            "name = \"demo\"\npath = \"/tmp/demo\"\nremote = \"origin\"\nbranch = \"master\"\nbuild_strategy = \"rust\"\nbuild_command = \"\"\n",
        )
        .unwrap();
        assert_eq!(parsed.build_strategy.as_deref(), Some("rust"));
        assert_eq!(
            parsed.resolved_build_strategy().unwrap(),
            BuildStrategyKind::Rust
        );
    }
}
