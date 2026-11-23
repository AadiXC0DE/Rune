//! A cache root is writable under the sandbox, and its neighbours are not.
#![cfg(any(target_os = "macos", target_os = "linux"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::io::Write as _;
use std::time::Duration;

use camino::Utf8PathBuf;
use rune_exec::{SandboxPolicy, detect, prepare_shell, run};

#[test]
fn a_cache_root_is_writable_and_its_neighbours_are_not() {
    let backend = detect();
    let support = backend.support();
    if !support.is_full() {
        writeln!(
            std::io::stderr(),
            "SKIPPED cache root test; no sandbox backend: {}",
            support.reason().unwrap_or("unknown")
        )
        .expect("skip report");
        return;
    }
    // The home is under the build directory, not the system temporary
    // directory, which the sandbox lets every command write.
    let base = Utf8PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "cache-roots-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    let home = base.join("home");
    std::fs::create_dir_all(home.join(".cache")).expect("cache");
    std::fs::create_dir_all(home.join("sibling")).expect("sibling");
    let workspace = base.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");

    let policy = SandboxPolicy::new(workspace.clone(), Vec::new(), false)
        .with_cache_roots(vec![home.join(".cache")]);
    let mut environment = BTreeMap::new();
    environment.insert(
        "PATH".to_owned(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned()),
    );
    environment.insert("H".to_owned(), home.to_string());
    let prepared = prepare_shell(
        r#"touch "$H/.cache/ok" && touch "$H/sibling/no""#,
        &workspace,
        None,
        environment,
    )
    .expect("prepare");
    let wrapped = backend.wrap(&prepared, &policy, false).expect("wrap");
    let outcome = run(&wrapped, Duration::from_secs(30), &|| false).expect("run");

    assert!(
        home.join(".cache/ok").exists(),
        "the cache root was not writable"
    );
    assert!(
        !home.join("sibling/no").exists(),
        "a directory beside the cache root was writable"
    );
    assert!(!outcome.exit.is_success(), "{outcome:?}");
    let _ = std::fs::remove_dir_all(&base);
}
