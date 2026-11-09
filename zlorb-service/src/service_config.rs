use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ServiceConfig {
    pub sleep_time: u64,
    username: String,
    token: String,
}

impl ServiceConfig {
    pub fn credentials(&self) -> Option<(String, String)> {
        let username = self.username.trim();
        let token = self.token.trim();
        if username.is_empty()
            || token.is_empty()
            || username == "NO USERNAME PROVIDED"
            || token == "NO TOKEN PROVIDED"
        {
            return None;
        }
        Some((username.to_string(), token.to_string()))
    }
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            sleep_time: 5,
            username: "NO USERNAME PROVIDED".into(),
            token: "NO TOKEN PROVIDED".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(username: &str, token: &str) -> ServiceConfig {
        ServiceConfig {
            sleep_time: 5,
            username: username.to_string(),
            token: token.to_string(),
        }
    }

    #[test]
    fn default_config_has_no_credentials() {
        assert_eq!(ServiceConfig::default().credentials(), None);
    }

    #[test]
    fn configured_pair_is_returned_trimmed() {
        assert_eq!(
            config("  alice ", " secret ").credentials(),
            Some(("alice".to_string(), "secret".to_string()))
        );
    }

    #[test]
    fn placeholder_or_blank_side_means_no_credentials() {
        assert_eq!(config("NO USERNAME PROVIDED", "secret").credentials(), None);
        assert_eq!(config("alice", "NO TOKEN PROVIDED").credentials(), None);
        assert_eq!(config("", "secret").credentials(), None);
        assert_eq!(config("alice", "  ").credentials(), None);
    }
}
