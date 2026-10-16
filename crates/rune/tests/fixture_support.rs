//! Regression coverage for the shared Python PTY fixture driver.

#![cfg(unix)]
#![allow(clippy::expect_used)]

#[test]
fn pty_drain_handles_eof_quiet_output_and_shutdown() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            concat!(
                include_str!("fixture_support.py"),
                "\n",
                include_str!("fixture_support_test.py")
            ),
        ])
        .output()
        .expect("python3 is required for the Unix PTY driver tests");
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "PTY driver tests failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
