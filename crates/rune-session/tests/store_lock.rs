//! Cross-process locking and durability.
//!
//! The writer lock only means anything across processes, and an in-process
//! mutex would pass every single-process test. This suite re-executes its own
//! binary as a child, so the lock is exercised through the same syscalls a
//! second `rune` invocation would use.

// Integration tests assert by panicking.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::process::{Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::ErrorCode;
use rune_core::id::{EventSeq, SessionId};
use rune_core::paths::Paths;
use rune_session::event::SessionEvent;
use rune_session::store::{LockHolder, SessionStore, load_read_only};
use rune_session::{EVENTS_FILE, LOCK_FILE};

/// Set in the child process to select the operation it performs.
const CHILD_MODE: &str = "RUNE_SESSION_CHILD_MODE";
/// Session directory the child opens.
const CHILD_DIR: &str = "RUNE_SESSION_CHILD_DIR";
/// Child test name, so the child runs only this test.
const CHILD_TEST: &str = "child_process_probe";

fn paths_for(root: &tempfile::TempDir) -> Paths {
    let base = |name: &str| Utf8PathBuf::from_path_buf(root.path().join(name)).expect("utf8 path");
    Paths {
        config_root: base("config"),
        state_root: base("state"),
        data_root: base("data"),
    }
}

/// Reads the holder record a lock file currently carries.
///
/// Returns `None` while the record is being replaced: the file is truncated and
/// then rewritten, so a process racing the holder can observe it empty. A writer
/// that is refused always reads a complete record, because the holder finishes
/// writing before it starts appending.
fn holder_record(dir: &Utf8Path) -> Option<LockHolder> {
    let text = std::fs::read_to_string(dir.join(LOCK_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Reads the holder record, requiring one to be present.
fn holder(dir: &Utf8Path) -> LockHolder {
    holder_record(dir).unwrap_or_else(|| panic!("lock file at `{dir}` carries no holder record"))
}

/// Extracts the single marker line a child prints.
fn marker(stdout: &str) -> String {
    stdout
        .lines()
        .find(|line| line.starts_with("ACQUIRED ") || line.starts_with("LOCKED "))
        .unwrap_or_else(|| panic!("child printed no marker: {stdout}"))
        .to_owned()
}

/// Runs this test binary as a child performing one short session operation.
fn spawn_child(mode: &str, dir: &Utf8Path) -> String {
    let output = Command::new(std::env::current_exe().expect("current exe"))
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .env(CHILD_MODE, mode)
        .env(CHILD_DIR, dir.as_str())
        .output()
        .expect("spawn child");
    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    marker(&String::from_utf8_lossy(&output.stdout))
}

/// Starts a child that acquires the lock and then holds it until killed.
///
/// Readiness is observed through the lock file rather than through the child's
/// stdout, so the parent proceeds only once the child really holds the lock.
fn spawn_holder(dir: &Utf8Path) -> std::process::Child {
    let mut child = Command::new(std::env::current_exe().expect("current exe"))
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .env(CHILD_MODE, "hold")
        .env(CHILD_DIR, dir.as_str())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn holder");

    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(10) {
        if holder_record(dir).is_some_and(|record| record.pid == child.id()) {
            return child;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("holder did not take the lock");
}

/// The child process entry point.
///
/// Does nothing when the mode variable is absent, so the ordinary parent run of
/// this test asserts nothing.
#[test]
fn child_process_probe() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let dir = Utf8PathBuf::from(std::env::var(CHILD_DIR).expect("child dir"));

    match mode.as_str() {
        "write" => match SessionStore::open_with_deadline(&dir, 0) {
            Ok(store) => println!("ACQUIRED {}", store.next_seq()),
            Err(err) => println!("LOCKED {}", err.message()),
        },
        "hold" => match SessionStore::open_with_deadline(&dir, 0) {
            Ok(store) => {
                println!("ACQUIRED held");
                std::thread::sleep(std::time::Duration::from_secs(60));
                drop(store);
            }
            Err(err) => println!("LOCKED {}", err.message()),
        },
        other => panic!("unknown child mode `{other}`"),
    }
}

#[test]
fn a_writer_in_another_process_is_refused_and_readers_are_not() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths_for(&root);
    let id = "sessionaaaaa".parse::<SessionId>().expect("id");
    let store = SessionStore::create(&paths, &id).expect("create");
    store
        .append(SessionEvent::TurnStarted { turn: 1 })
        .expect("append");
    store
        .append(SessionEvent::UserMessage {
            text: "held by the parent".to_owned(),
        })
        .expect("append");
    let dir = store.dir().to_path_buf();

    let locked = spawn_child("write", &dir);
    assert!(locked.starts_with("LOCKED "), "child said `{locked}`");
    assert!(locked.contains("sessionaaaaa"), "child said `{locked}`");
    assert!(
        locked.contains(&std::process::id().to_string()),
        "child did not name the holder: `{locked}`"
    );
    assert!(locked.contains("locked"), "child said `{locked}`");

    // The lock is the writer's alone: the log is unchanged and still readable.
    assert_eq!(load_read_only(&dir).expect("read").len(), 2);

    drop(store);
    let acquired = spawn_child("write", &dir);
    assert_eq!(acquired, "ACQUIRED 3", "the lock did not pass to the child");
}

#[test]
fn a_writer_killed_while_holding_the_lock_releases_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths_for(&root);
    let id = "sessionbbbbb".parse::<SessionId>().expect("id");
    let dir = paths.session_dir(&id);
    {
        let store = SessionStore::create(&paths, &id).expect("create");
        store
            .append(SessionEvent::UserMessage {
                text: "before the crash".to_owned(),
            })
            .expect("append");
    }

    let mut victim = spawn_holder(&dir);
    assert_ne!(holder(&dir).pid, std::process::id());
    assert_eq!(
        SessionStore::open_with_deadline(&dir, 0)
            .expect_err("held by the child")
            .code(),
        ErrorCode::Locked
    );
    // A reader is unaffected by another process holding the write lock.
    assert_eq!(load_read_only(&dir).expect("read").len(), 1);

    victim.kill().expect("kill holder");
    victim.wait().expect("reap holder");

    let reopened = SessionStore::open(&dir).expect("open after a killed writer");
    assert_eq!(holder(&dir).pid, std::process::id());
    assert_eq!(
        reopened
            .append(SessionEvent::TitleSet {
                title: "after the crash".to_owned(),
            })
            .expect("append"),
        EventSeq(2)
    );
    assert_eq!(reopened.read().expect("read").len(), 2);
}

#[test]
fn a_stale_holder_record_names_the_writer_that_holds_the_lock() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths_for(&root);
    let id = "sessionccccc".parse::<SessionId>().expect("id");
    let store = SessionStore::create(&paths, &id).expect("create");
    let dir = store.dir().to_path_buf();

    assert_eq!(holder(&dir).pid, std::process::id());
    assert!(holder(&dir).since_ms > 0);
    assert!(std::fs::metadata(dir.join(EVENTS_FILE)).is_ok());

    // The record is what a refused writer reports, so write one that names a
    // process no longer running.
    let stale = LockHolder {
        pid: u32::MAX.saturating_sub(1),
        host: None,
        since_ms: 1,
    };
    std::fs::write(
        dir.join(LOCK_FILE),
        serde_json::to_string(&stale).expect("json"),
    )
    .expect("write");

    let err = SessionStore::open_with_deadline(&dir, 0).expect_err("still locked");
    assert_eq!(err.code(), ErrorCode::Locked);
    assert!(
        err.message().contains(&stale.pid.to_string()),
        "{}",
        err.message()
    );

    // The lock itself is released on drop, and the next writer overwrites the
    // stale record rather than being wedged by it.
    drop(store);
    let third = SessionStore::open_with_deadline(&dir, 0).expect("open");
    assert_eq!(holder(&dir).pid, std::process::id());
    drop(third);
}
