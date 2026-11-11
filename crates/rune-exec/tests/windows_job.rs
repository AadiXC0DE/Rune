//! A stopped Windows command takes every process it started with it.
#![cfg(windows)]
#![allow(clippy::expect_used, clippy::panic)]

use std::process::Command;
use std::time::{Duration, Instant};

use rune_exec::Process;
use rune_exec::session::Input;

/// Ids of the running `PING.EXE` processes.
fn running_pings() -> Vec<u32> {
    let output = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq PING.EXE", "/FO", "CSV", "/NH"])
        .output()
        .expect("tasklist");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split("\",\"");
            let image = fields.next()?.trim_start_matches('"');
            let id = fields.next()?;
            image
                .eq_ignore_ascii_case("PING.EXE")
                .then(|| id.parse().ok())
                .flatten()
        })
        .collect()
}

#[test]
fn stopping_a_command_stops_its_children() {
    let before = running_pings();
    let process = Process::start(
        "start /b ping -t 127.0.0.1 & ping -t 127.0.0.1",
        None,
        4096,
        Input::Closed,
    )
    .expect("start");
    let deadline = Instant::now() + Duration::from_secs(20);
    let started = loop {
        let pings: Vec<u32> = running_pings()
            .into_iter()
            .filter(|id| !before.contains(id))
            .collect();
        let in_job = process.job_process_ids();
        let ours: Vec<u32> = pings.into_iter().filter(|id| in_job.contains(id)).collect();
        if ours.len() >= 2 {
            break ours;
        }
        assert!(
            Instant::now() < deadline,
            "both ping processes did not start in the job: {in_job:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    drop(process);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left: Vec<u32> = running_pings()
            .into_iter()
            .filter(|id| started.contains(id))
            .collect();
        if left.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ping processes outlived the command: {left:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
