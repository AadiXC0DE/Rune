//! R-161 regression through the CLI with isolated skill roots and a local provider.

#![allow(clippy::expect_used)]

mod support;

#[test]
fn discovered_skills_can_be_searched_and_loaded_with_configured_bounds() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(concat!(
                include_str!("fixture_support.py"),
                "\n",
                include_str!("skills.py")
            )),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the local skill fixture");
    // The fixture names each phase and reports every subprocess deadline, so a
    // loaded runner explains itself instead of showing only the test name.
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "skill regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
