//! Shell completions are a stdout-only command: no daemon, no files and no side effects.

use std::io::Read;
use std::process::{Command, Stdio};

#[test]
fn every_supported_shell_receives_its_own_script() {
    let cases = [
        ("bash", "_tidemarkctl()"),
        ("zsh", "#compdef tidemarkctl"),
        ("fish", "complete -c tidemarkctl"),
    ];

    for (shell, marker) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_tidemarkctl"))
            .args(["completions", shell])
            .output()
            .expect("tidemarkctl runs");

        assert!(output.status.success(), "{shell}: {output:?}");
        let stdout = String::from_utf8(output.stdout).expect("completion script is UTF-8");
        assert!(
            stdout.contains(marker),
            "{shell} completion did not contain {marker:?}: {stdout}"
        );
        assert!(stdout.contains("provider"), "{shell}: {stdout}");
    }
}

#[test]
fn a_reader_that_stops_early_does_not_make_the_cli_panic() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tidemarkctl"))
        .args(["completions", "bash"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("tidemarkctl starts");
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut first = [0_u8; 1];
    stdout.read_exact(&mut first).expect("script starts");
    drop(stdout);

    let output = child.wait_with_output().expect("tidemarkctl exits");
    assert!(output.status.success(), "{output:?}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("panicked"),
        "{output:?}"
    );
}
