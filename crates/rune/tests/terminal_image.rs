//! Images attached by paste and by `/image`, through the real binary and a PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

fn scenario(name: &str) {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/attachment.png");
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_image.py")),
            env!("CARGO_BIN_EXE_rune"),
            fixture,
            name,
        ])
        .output()
        .expect("python3 is required for the Unix image test");
    assert!(
        output.status.success(),
        "image scenario {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_pasted_image_path_reaches_the_model() {
    scenario("paste");
}

#[test]
fn image_refuses_a_file_that_is_not_an_image() {
    scenario("refuse");
}
