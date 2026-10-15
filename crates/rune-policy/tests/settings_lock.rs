//! Exercise settings locks through real writer processes, including process death.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use rune_core::error::ErrorCode;
use rune_policy::settings;

const CHILD_TEST: &str = "settings_writer_child";
const CHILD_MODE: &str = "RUNE_SETTINGS_TEST_MODE";
const CHILD_PATH: &str = "RUNE_SETTINGS_TEST_PATH";

struct WriterChild(Child);

impl Drop for WriterChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child_command(mode: &str, path: &Utf8PathBuf) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .env(CHILD_MODE, mode)
        .env(CHILD_PATH, path.as_str());
    command
}

#[test]
fn settings_writer_child() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let path = Utf8PathBuf::from(std::env::var(CHILD_PATH).expect("settings path"));
    match mode.as_str() {
        "hold" => {
            settings::update(&path, |current| {
                assert_eq!(current, Some("{\"before\":true}\n"));
                // This runs only after update has acquired the settings lock.
                std::fs::write(path.with_extension("ready"), "ready").expect("signal readiness");
                std::thread::sleep(Duration::from_secs(60));
                Ok("{}\n".to_owned())
            })
            .expect("holding writer");
        }
        "write" => {
            settings::write(&path, "{\"after\":true}\n", None).expect("write after process death");
        }
        "update" => {
            settings::update(&path, |current| {
                assert_eq!(current, Some("{\"before\":true}\n"));
                Ok("{\"after\":true}\n".to_owned())
            })
            .expect("update after process death");
        }
        other => panic!("unexpected child mode {other}"),
    }
}

#[test]
fn a_killed_writer_releases_the_lock_for_write_and_update() {
    for mode in ["write", "update"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("settings.json")).expect("utf8");
        settings::write(&path, "{\"before\":true}\n", None).expect("initial settings");
        let mut holder = WriterChild(
            child_command("hold", &path)
                .stdout(Stdio::null())
                .spawn()
                .expect("spawn holder"),
        );
        let started = Instant::now();
        while !path.with_extension("ready").exists() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "holder never became ready"
            );
            assert!(
                holder.0.try_wait().expect("check holder").is_none(),
                "holder exited early"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let err = settings::write(&path, "{}\n", None).expect_err("live holder excludes writers");
        assert_eq!(err.code(), ErrorCode::Locked);
        assert_eq!(
            err.detail().observed.as_deref(),
            Some(holder.0.id().to_string().as_str())
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "{\"before\":true}\n"
        );

        holder
            .0
            .kill()
            .expect("kill writer without dropping its lock");
        holder.0.wait().expect("reap writer");
        assert!(
            settings::lock_path_for(&path).exists(),
            "killed writer left its lock file"
        );

        let output = child_command(mode, &path)
            .output()
            .expect("spawn next writer");
        assert!(
            output.status.success(),
            "{mode} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "{\"after\":true}\n"
        );
    }
}
