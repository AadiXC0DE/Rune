//! Kill a real session writer while its metadata replacement is being staged.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use rune_core::id::{EventSeq, SessionId};
use rune_core::paths::Paths;
use rune_session::store::METADATA_FILE;
use rune_session::{Metadata, SessionEvent, SessionStore, load_read_only};

const CHILD_DIR: &str = "RUNE_METADATA_TEST_DIR";
const CHILD_TEST: &str = "metadata_writer_child";
const TITLE_BYTES: usize = 4 * 1024 * 1024;

struct WriterChild(Child);

impl Drop for WriterChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn metadata_writer_child() {
    let Ok(dir) = std::env::var(CHILD_DIR) else {
        return;
    };
    let dir = Utf8PathBuf::from(dir);
    let store = SessionStore::open(&dir).expect("open child session");
    // Only observe staging after open has finished rebuilding the metadata.
    std::fs::write(dir.join("ready"), "ready").expect("signal readiness");
    for _ in 0..8 {
        store
            .append(SessionEvent::TitleSet {
                title: "x".repeat(TITLE_BYTES),
            })
            .expect("append child title");
    }
}

#[test]
fn killing_a_metadata_writer_leaves_complete_metadata_and_replayable_events() {
    let root = tempfile::tempdir().expect("tempdir");
    let base = |name| Utf8PathBuf::from_path_buf(root.path().join(name)).expect("utf8");
    let paths = Paths {
        config_root: base("config"),
        state_root: base("state"),
        data_root: base("data"),
    };
    let id = "sessionaaaaa".parse::<SessionId>().expect("id");
    let dir = paths.session_dir(&id);
    {
        let store = SessionStore::create(&paths, &id).expect("create");
        store
            .append(SessionEvent::UserMessage {
                text: "before the crash".to_owned(),
            })
            .expect("append initial event");
    }
    let mut writer = WriterChild(
        Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", CHILD_TEST, "--nocapture"])
            .env(CHILD_DIR, dir.as_str())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn metadata writer"),
    );

    let started = Instant::now();
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "writer never staged a metadata replacement"
        );
        assert!(
            writer.0.try_wait().expect("child status").is_none(),
            "writer exited before a replacement was observed"
        );
        if dir.join("ready").exists()
            && std::fs::read_dir(&dir)
                .expect("session directory")
                .any(|entry| {
                    let name = entry.expect("directory entry").file_name();
                    let name = name.to_string_lossy();
                    name.starts_with(".rune-private-") && name.ends_with(".tmp")
                })
        {
            break;
        }
        std::thread::yield_now();
    }
    writer.0.kill().expect("kill writer during replacement");
    assert!(!writer.0.wait().expect("reap writer").success());

    let metadata: Metadata =
        serde_json::from_slice(&std::fs::read(dir.join(METADATA_FILE)).expect("read metadata"))
            .expect("complete metadata after process death");
    let state = load_read_only(&dir).expect("replay authoritative events");
    assert!(state.len() >= 2, "the staged projection follows event sync");
    assert_eq!(state.events[0].seq, EventSeq::FIRST);
    assert_eq!(metadata.id, id);
    assert_eq!(metadata.events, metadata.last_seq);
    let durable_events = u64::try_from(state.len()).expect("event count");
    assert!(
        metadata.events == durable_events || metadata.events + 1 == durable_events,
        "metadata must describe the old or new durable boundary"
    );
    if metadata.events == 1 {
        assert_eq!(metadata.title, None);
    } else {
        assert_eq!(
            metadata.title.as_deref(),
            Some("x".repeat(TITLE_BYTES).as_str())
        );
    }
    assert_eq!(
        state.title.as_deref(),
        Some("x".repeat(TITLE_BYTES).as_str())
    );

    let reopened = SessionStore::open(&dir).expect("reopen killed writer's session");
    assert_eq!(reopened.next_seq(), EventSeq(durable_events + 1));
    let repaired: Metadata =
        serde_json::from_slice(&std::fs::read(reopened.metadata_path()).expect("rebuilt metadata"))
            .expect("complete rebuilt metadata");
    assert_eq!(repaired, reopened.metadata());
    assert_eq!(repaired.events, durable_events);
    assert_eq!(
        reopened
            .append(SessionEvent::UserMessage {
                text: "after the crash".to_owned(),
            })
            .expect("append after process death"),
        EventSeq(durable_events + 1)
    );
    assert_eq!(
        reopened.read().expect("replay after append").len(),
        state.len() + 1
    );
}
