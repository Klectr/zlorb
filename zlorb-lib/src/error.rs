use std::{error::Error, fmt, path::PathBuf};

#[derive(Debug)]
pub enum ZlorbError {
    Io(std::io::Error),
    HomeDirNotFound,
    ConfigNotFound(PathBuf),
    ConfigParseError(String),
    InvalidConfig(String),
    FileNotFound(PathBuf),
    PermissionDenied(String),
    SerializationErrorGeneric(String),
    SerializationError(serde_json::Error),
    TomlSerializationError(toml::ser::Error),
    TomlDeserializationError(toml::de::Error),
    Git(git2::Error),
    Other(String),
}

fn strip_escape_sequences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some('[') {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl ZlorbError {
    pub fn error_line(&self) -> String {
        let rendered = strip_escape_sequences(&self.to_string());
        let parts: Vec<&str> = rendered
            .split(['\n', '\r'])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        format!("error: {}", parts.join(" | "))
    }

    pub fn print(&self) {
        eprintln!("{}", self.error_line());
    }
}

impl fmt::Display for ZlorbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZlorbError::Io(error) => write!(f, "IO error: {error}"),
            ZlorbError::HomeDirNotFound => write!(f, "Could not find home directory"),
            ZlorbError::ConfigNotFound(path) => {
                write!(f, "Config file not found: {path:?}")
            }
            ZlorbError::ConfigParseError(msg) => {
                write!(f, "Failed to parse config: {msg}")
            }
            ZlorbError::InvalidConfig(msg) => {
                write!(f, "Invalid configuration: {msg}")
            }
            ZlorbError::FileNotFound(path) => write!(f, "File not found: {path:?}"),
            ZlorbError::PermissionDenied(path) => {
                write!(f, "Permission denied: {path:?}")
            }
            ZlorbError::SerializationErrorGeneric(msg) => {
                write!(f, "Generic Serialization error: {msg}")
            }
            ZlorbError::Other(msg) => write!(f, "Other error occured: {msg}"),
            ZlorbError::Git(git) => write!(f, "Recieved git error: {git}"),
            ZlorbError::SerializationError(error) => {
                write!(f, "Serialization error: {error}")
            }
            ZlorbError::TomlSerializationError(error) => {
                write!(f, "Toml Serialization error: {error}")
            }
            ZlorbError::TomlDeserializationError(error) => {
                write!(f, "Toml Deserialization error: {error}")
            }
        }
    }
}

impl Error for ZlorbError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ZlorbError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ZlorbError {
    fn from(value: std::io::Error) -> Self {
        ZlorbError::Io(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_writes_the_message_itself() {
        let err = ZlorbError::Other("boom".to_string());
        assert_eq!(format!("{}", err), "Other error occured: boom");
        let err = ZlorbError::InvalidConfig("bad branch".to_string());
        assert_eq!(format!("{}", err), "Invalid configuration: bad branch");
    }

    #[test]
    fn error_line_is_one_plain_line_even_for_a_multiline_message() {
        let err = ZlorbError::ConfigParseError("first line\nsecond line\r\nthird".to_string());
        let line = err.error_line();
        assert_eq!(
            line,
            "error: Failed to parse config: first line | second line | third"
        );
        assert!(!line.contains('\n'), "{}", line);
        assert!(!line.contains('\x1b'), "{}", line);
    }

    #[test]
    fn error_line_strips_escape_sequences_embedded_in_the_message() {
        let err = ZlorbError::Other("\x1b[31mboom\x1b[0m".to_string());
        assert_eq!(err.error_line(), "error: Other error occured: boom");
    }

    #[test]
    fn error_line_prefixes_a_single_line_message() {
        let err = ZlorbError::Other("boom".to_string());
        assert_eq!(err.error_line(), "error: Other error occured: boom");
    }
}
