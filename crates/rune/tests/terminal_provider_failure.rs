//! R-025 regression through the real binary, a PTY, and a local provider.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

#[test]
fn provider_failure_saves_the_visible_answer_and_boundary_across_resume() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(concat!(
                include_str!("fixture_support.py"),
                "\n",
                include_str!("terminal_provider_failure.py")
            )),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix terminal provider failure regression");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "terminal provider failure regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
