//! The service's configuration file: `KEY=VALUE`, one per line.
//!
//! The same format on every platform. The login tool writes it and the
//! service reads it once at startup to build the host's environment and its
//! arguments, so parsing is cold: one pass per process, and allocation here
//! never reaches a data path.

#[cfg(windows)]
use std::path::PathBuf;

/// Parse the file's lines into key and value pairs, in the order they appear.
///
/// A line whose first character is `#` is a comment, a blank line is skipped,
/// and the first `=` splits the rest, so a value may contain `=`. The key and
/// the value are trimmed of the whitespace around them; nothing is unquoted,
/// because the value is handed on as it is. A line with no `=` is skipped
/// rather than refused: people edit this file by hand, and one stray line must
/// not take the host down.
pub fn parse(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        pairs.push((key.to_string(), value.trim().to_string()));
    }
    pairs
}

/// Where the Windows service keeps its configuration.
///
/// `%ProgramData%\lowlat\lowlatd.env`, the folder install protects: the file
/// carries the signaling secret, so it is readable by the system account and
/// administrators alone. `None` when the environment does not name the
/// program-data directory, which a Windows process is not started without.
#[cfg(windows)]
pub fn service_path() -> Option<PathBuf> {
    std::env::var_os("ProgramData")
        .map(|base| PathBuf::from(base).join("lowlat").join("lowlatd.env"))
}

#[cfg(test)]
mod tests {
    use super::parse;

    /// Comments, blank lines and Windows line endings are all skipped or
    /// stripped, and the order is the file's.
    #[test]
    fn comments_blanks_and_crlf_are_skipped() {
        let text = "# a comment\r\n\r\nKESSEL_SESSION=abc\r\n  # indented\r\nKESSEL_WS_SERVER=wss://one\r\n";
        assert_eq!(
            parse(text),
            vec![
                ("KESSEL_SESSION".into(), "abc".into()),
                ("KESSEL_WS_SERVER".into(), "wss://one".into()),
            ]
        );
    }

    /// The first `=` splits and everything after it, `=` included, is the
    /// value; the whitespace around both is dropped.
    #[test]
    fn the_first_equals_splits_and_the_rest_is_the_value() {
        let text = "LOWLATD_ARGS=--name \"a=b\" --synth";
        assert_eq!(
            parse(text),
            vec![("LOWLATD_ARGS".into(), "--name \"a=b\" --synth".into())]
        );
    }

    /// A line with no `=`, and a line whose key is empty, are skipped.
    #[test]
    fn lines_without_a_key_are_skipped() {
        let text = "KESSEL_SESSION=\nnot a pair\n=value\n";
        assert_eq!(parse(text), vec![("KESSEL_SESSION".into(), String::new())]);
    }
}
