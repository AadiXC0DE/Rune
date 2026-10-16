//! R-162 regression through interactive and ask hosts with a local vision fixture.

#![allow(clippy::expect_used)]

mod support;

#[test]
fn configured_vision_uploads_permitted_images_and_unavailable_runs_are_labelled() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("vision.py")),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the local vision fixture");
    assert!(
        output.status.success(),
        "vision regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
