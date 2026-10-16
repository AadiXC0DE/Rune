//! R-008 regression through the real binary, a PTY, and a local provider.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

#[test]
fn structured_questions_collect_terminal_answers_for_the_next_provider_request() {
    // Python's standard library supplies the PTY without a dependency or unsafe
    // code. The fixture has deadlines and reaps its child on failure.
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(concat!(
                include_str!("fixture_support.py"),
                "\n",
                include_str!("terminal_question.py")
            )),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix terminal question regression");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "terminal question regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
