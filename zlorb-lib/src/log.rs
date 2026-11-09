use std::io::IsTerminal;

const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m"; // Reset to default

pub struct Logger {}

fn format_line(level: &str, color: &str, use_color: bool, msg: &str) -> String {
    if use_color {
        format!("[{}{}{}]: {}", color, level, RESET, msg)
    } else {
        format!("[{}]: {}", level, msg)
    }
}

impl Logger {
    pub fn info(msg: String) {
        println!(
            "{}",
            format_line("INFO", GREEN, std::io::stdout().is_terminal(), &msg)
        );
    }

    pub fn error(msg: String) {
        eprintln!(
            "{}",
            format_line("ERROR", RED, std::io::stderr().is_terminal(), &msg)
        );
    }

    pub fn warn(msg: String) {
        eprintln!(
            "{}",
            format_line("WARN", YELLOW, std::io::stderr().is_terminal(), &msg)
        );
    }

    pub fn debug(msg: String) {
        println!(
            "{}",
            format_line("DEBUG", YELLOW, std::io::stdout().is_terminal(), &msg)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_line_carries_no_escape_codes() {
        let line = format_line("ERROR", RED, false, "boom");
        assert_eq!(line, "[ERROR]: boom");
        assert!(!line.contains('\x1b'), "{}", line);
    }

    #[test]
    fn coloured_line_wraps_only_the_level() {
        let line = format_line("INFO", GREEN, true, "hello");
        assert_eq!(line, "[\x1b[32mINFO\x1b[0m]: hello");
    }
}
