//! Where a secret is allowed to come from.
//!
//! stdin or a file, never an argv value: `/proc/<pid>/cmdline` is readable by every process
//! of the same user, and a key typed once ends up in shell history. stdin is a file
//! descriptor rather than a terminal, so a plugin with its own UI writes into a pipe and
//! never opens a console.

use std::io::Read;
use std::path::PathBuf;

use crate::exit::Failure;

#[derive(Debug)]
pub enum Source {
    Stdin,
    File(PathBuf),
}

/// One trailing newline is trimmed — `printf` users and `echo` users should get the same
/// key — and nothing else is touched: a key with meaningful interior characters is the
/// provider's business, not ours.
pub fn read(source: Source) -> Result<String, Failure> {
    let raw = match source {
        Source::Stdin => {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .map_err(|error| Failure::usage(format!("cannot read stdin: {error}")))?;
            buffer
        }
        Source::File(path) => std::fs::read_to_string(&path)
            .map_err(|error| Failure::usage(format!("cannot read {}: {error}", path.display())))?,
    };
    let trimmed = raw
        .strip_suffix('\n')
        .map(|value| value.strip_suffix('\r').unwrap_or(value))
        .unwrap_or(&raw);
    // An empty value is refused here rather than sent: `SetKey("")` is a request to store
    // nothing, which the daemon refuses anyway, and this way the user gets a sentence
    // saying what to do instead of a D-Bus error to interpret.
    if trimmed.is_empty() {
        return Err(Failure::usage(
            "no value on stdin: pipe the key in, or pass --key-file",
        ));
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, contents: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("tidemarkctl-secret-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).expect("writes");
        path
    }

    #[test]
    fn one_trailing_newline_is_trimmed() {
        let path = file("echo", "sk-abc\n");
        assert_eq!(read(Source::File(path.clone())).expect("reads"), "sk-abc");
        std::fs::remove_file(path).ok();
    }

    /// `echo` adds one newline, so one is noise. A second one is a character somebody put
    /// there, and a credential is not ours to tidy.
    #[test]
    fn a_second_trailing_newline_belongs_to_the_value() {
        let path = file("twice", "sk-abcd\n\n");
        assert_eq!(
            read(Source::File(path.clone())).expect("reads"),
            "sk-abcd\n"
        );
        std::fs::remove_file(path).ok();
    }

    /// A CRLF file is a Windows file, and the carriage return is the line ending rather
    /// than part of the key.
    #[test]
    fn a_windows_line_ending_is_not_part_of_the_key() {
        let path = file("crlf", "sk-abc\r\n");
        assert_eq!(read(Source::File(path.clone())).expect("reads"), "sk-abc");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn nothing_at_all_is_a_usage_error_not_a_cleared_key() {
        let path = file("empty", "\n");
        let failure = read(Source::File(path.clone())).expect_err("refuses");
        assert_eq!(failure.exit, crate::exit::Exit::Usage);
        std::fs::remove_file(path).ok();
    }

    /// The message names the path, because the usual cause is a typo in it.
    #[test]
    fn a_missing_file_says_which_one() {
        let path = std::env::temp_dir().join("tidemarkctl-secret-does-not-exist");
        std::fs::remove_file(&path).ok();
        let failure = read(Source::File(path.clone())).expect_err("refuses");
        assert_eq!(failure.exit, crate::exit::Exit::Usage);
        assert!(
            failure
                .message
                .contains("tidemarkctl-secret-does-not-exist"),
            "{}",
            failure.message
        );
    }
}
