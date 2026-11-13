//! Shift+Tab cycles ask, auto and plan; the prompt names plan mode in words
//! and the model is told through the next prompt.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use rune_testkit::scripted::{ScriptedProvider, Turn};

fn screen(bytes: &[u8]) -> String {
    let mut grid = rune_term::Grid::new(80, 24).expect("grid");
    grid.feed(bytes).expect("replay");
    grid.text()
}

#[test]
fn shift_tab_cycles_the_mode_and_the_prompt_shows_it() {
    let provider = ScriptedProvider::start(vec![Turn::text("First.")]);
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_mode.py")),
            env!("CARGO_BIN_EXE_rune"),
            &provider.base_url(),
        ])
        .output()
        .expect("python3 is required for the Unix terminal mode test");
    assert!(
        output.status.success(),
        "terminal mode failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&output.stdout).expect("captures");
    assert_eq!(captures.len(), 2);
    let prompt_row = |screen: &str| -> String {
        screen
            .lines()
            .skip_while(|row| !row.starts_with("scripted | "))
            .nth(1)
            .expect("prompt row")
            .to_owned()
    };
    let status_row = |screen: &str| -> String {
        screen
            .lines()
            .find(|row| row.starts_with("scripted | "))
            .expect("status row")
            .to_owned()
    };
    let plan = screen(&captures[0].1);
    assert_eq!(prompt_row(&plan).trim_end(), "plan >", "{plan}");
    assert!(status_row(&plan).starts_with("scripted | plan |"), "{plan}");
    assert!(plan.contains("shift-tab mode"), "{plan}");
    let ask = screen(&captures[1].1);
    assert_eq!(prompt_row(&ask).trim_end(), ">", "{ask}");
    assert!(status_row(&ask).starts_with("scripted | ask |"), "{ask}");

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).expect("request JSON");
    let user = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .expect("user message")
        .to_string();
    assert!(user.contains("<plan_mode>"), "{user}");
    assert!(user.contains("Plan mode is on"), "{user}");
    assert!(user.contains("Permission mode is now auto."), "{user}");
}
