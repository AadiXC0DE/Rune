//! R-070 acceptance through a real terminal and a local streaming provider.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

#[test]
fn accessible_sessions_append_labelled_output_without_terminal_controls() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(concat!(
                include_str!("fixture_support.py"),
                "\n",
                include_str!("terminal_accessible.py")
            )),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix accessible terminal test");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "accessible terminal acceptance failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
