//! Checkpoints of a session: a snapshot at each turn boundary, the timeline
//! `/undo` and `/redo` walk, and restores.
//!
//! The snapshots themselves live in a private git directory per workspace,
//! [`Shadow`], shared with embedders through `rune-session`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_session::event::{Boundary, RestoreDirection, SessionEvent};
pub use rune_session::shadow::{
    ChangeKind, FileChange, Shadow, Snapshot, remove_session, store_size,
};

/// Largest a workspace's checkpoint store may grow before old sessions are removed.
pub const STORE_CAP: u64 = 1_073_741_824;
/// Longest a restore waits for queued snapshots.
const RESTORE_WAIT: Duration = Duration::from_secs(60);

/// One checkpoint as the session log records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointRecord {
    pub turn: u64,
    pub boundary: Boundary,
    pub snapshot: Snapshot,
    pub complete: bool,
}

impl CheckpointRecord {
    fn event(&self) -> SessionEvent {
        SessionEvent::WorkspaceCheckpoint {
            turn: self.turn,
            boundary: self.boundary,
            tree: self.snapshot.tree.clone(),
            commit: self.snapshot.commit.clone(),
            parent: self.snapshot.parent.clone(),
            skipped: self.snapshot.skipped.clone(),
            complete: self.complete,
        }
    }
}

/// Shows a line in the transcript of the session that owns the checkpoints.
pub type Notify = Arc<dyn Fn(String) + Send + Sync>;

/// Appends a checkpoint event to the session log.
pub type Record = Arc<dyn Fn(SessionEvent) -> Result<()> + Send + Sync>;

/// The tree a turn started from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnStart {
    pub tree: String,
    /// False when the snapshot was not ready before the turn changed files.
    pub complete: bool,
}

/// A session's checkpoints and restores, in the order they happened.
#[derive(Clone, Debug, Default)]
pub struct Timeline {
    /// Tree or commit the workspace last held: the newest checkpoint or restore target.
    current: Option<String>,
    /// Every checkpoint tree with its turn, oldest first.
    trees: Vec<(u64, String)>,
    starts: BTreeMap<u64, TurnStart>,
    /// Turns whose start and end trees differ, oldest first.
    changed: Vec<u64>,
    undone: BTreeSet<u64>,
    /// Turn and saved commit of each undo not yet redone, oldest first.
    redo: Vec<(u64, String)>,
}

impl Timeline {
    /// Rebuilds the timeline from a session's events.
    pub fn from_events(events: &[SessionEvent]) -> Self {
        let mut timeline = Self::default();
        for event in events {
            timeline.apply(event);
        }
        timeline
    }

    /// Takes one event into account; events of other kinds are ignored.
    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::WorkspaceCheckpoint {
                turn,
                boundary,
                tree,
                complete,
                ..
            } => {
                match boundary {
                    Boundary::Start => {
                        self.starts.insert(
                            *turn,
                            TurnStart {
                                tree: tree.clone(),
                                complete: *complete,
                            },
                        );
                    }
                    Boundary::End => {
                        // A start identical to the previous checkpoint was not
                        // recorded, so the turn started from the current tree.
                        if !self.starts.contains_key(turn)
                            && let Some(current) = &self.current
                        {
                            self.starts.insert(
                                *turn,
                                TurnStart {
                                    tree: current.clone(),
                                    complete: true,
                                },
                            );
                        }
                        let changed = self
                            .starts
                            .get(turn)
                            .is_some_and(|start| start.tree != *tree);
                        if changed {
                            if !self.changed.contains(turn) {
                                self.changed.push(*turn);
                            }
                            self.undone.remove(turn);
                            self.redo.clear();
                        }
                    }
                }
                self.trees.push((*turn, tree.clone()));
                self.current = Some(tree.clone());
            }
            SessionEvent::Restored {
                turn,
                direction,
                target,
                ..
            } => {
                match direction {
                    RestoreDirection::Undo => {
                        self.undone.insert(*turn);
                        if let SessionEvent::Restored { saved, .. } = event {
                            self.redo.push((*turn, saved.clone()));
                        }
                    }
                    RestoreDirection::Redo => {
                        self.redo.pop();
                        self.undone.remove(turn);
                    }
                    RestoreDirection::Turn => {}
                }
                self.current = Some(target.clone());
            }
            _ => {}
        }
    }

    /// Turn and start tree of the newest turn that changed files and is not undone.
    pub fn undo_target(&self) -> Option<(u64, String)> {
        self.changed
            .iter()
            .rev()
            .find(|turn| !self.undone.contains(turn))
            .and_then(|turn| {
                self.starts
                    .get(turn)
                    .map(|start| (*turn, start.tree.clone()))
            })
    }

    /// Turn and saved commit of the newest undo not yet redone.
    pub fn redo_target(&self) -> Option<(u64, String)> {
        self.redo.last().cloned()
    }

    /// The start checkpoint recorded for a turn.
    pub fn turn_start(&self, turn: u64) -> Option<&TurnStart> {
        self.starts.get(&turn)
    }

    /// The tree the workspace held before `turn`: its start checkpoint, or the
    /// newest checkpoint of an earlier turn when the turn recorded none.
    pub fn before_turn(&self, turn: u64) -> Option<TurnStart> {
        if let Some(start) = self.starts.get(&turn) {
            return Some(start.clone());
        }
        self.trees
            .iter()
            .rev()
            .find(|(recorded, _)| *recorded < turn)
            .map(|(_, tree)| TurnStart {
                tree: tree.clone(),
                complete: true,
            })
    }

    /// The tree or commit the workspace was last known to hold.
    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }
}

/// A restore asked for: the turn it refers to, what to restore to, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreRequest {
    pub turn: u64,
    pub target: String,
    pub direction: RestoreDirection,
}

/// What a restore did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreReport {
    /// The files restored, listed as the turn changed them: an undo of a turn
    /// that created a file lists it as added. A redo lists what it puts back.
    Done(Vec<FileChange>),
    /// Files changed since the session's last checkpoint; nothing was written.
    OutsideChanges(Vec<FileChange>),
}

/// The refusal for a turn whose start snapshot was not ready in time.
pub fn incomplete(turn: u64) -> RuneError {
    RuneError::new(
        ErrorCode::InvalidState,
        format!(
            "the checkpoint for turn {turn} is incomplete (it was not ready before the turn changed files); restore refused"
        ),
    )
}

/// Restores the workspace for one request, the procedure every entry point shares.
///
/// Takes a snapshot first, so the state being replaced can be brought back, and
/// refuses without `force` when files changed since the session's last
/// checkpoint. A dry run returns the changes and writes nothing.
pub fn restore_to(
    shadow: &Shadow,
    session: &str,
    timeline: &mut Timeline,
    request: &RestoreRequest,
    force: bool,
    dry_run: bool,
    record: &Record,
) -> Result<RestoreReport> {
    if request.direction != RestoreDirection::Redo
        && timeline
            .before_turn(request.turn)
            .is_some_and(|start| !start.complete)
    {
        return Err(incomplete(request.turn));
    }
    let (before, skipped) = shadow.stage()?;
    if !force && let Some(current) = timeline.current() {
        let known = shadow.tree_of(current)?;
        if known != before {
            let outside = shadow.diff(&known, &before)?;
            if !outside.is_empty() {
                return Ok(RestoreReport::OutsideChanges(outside));
            }
        }
    }
    // Going back lists the turn's own changes; going forward lists what returns.
    let as_turn = |changes: Vec<FileChange>| -> Vec<FileChange> {
        if request.direction == RestoreDirection::Redo {
            changes
        } else {
            changes.iter().map(FileChange::reversed).collect()
        }
    };
    if dry_run {
        return Ok(RestoreReport::Done(as_turn(
            shadow.diff(&before, &request.target)?,
        )));
    }
    let saved = shadow.commit(session, "before restore", before.clone(), skipped)?;
    let changes = as_turn(shadow.restore(&before, &request.target)?);
    let event = SessionEvent::Restored {
        turn: request.turn,
        direction: request.direction,
        target: request.target.clone(),
        saved: saved.commit,
    };
    record(event.clone())?;
    timeline.apply(&event);
    Ok(RestoreReport::Done(changes))
}

/// One snapshot the worker is asked to take.
struct Job {
    turn: u64,
    boundary: Boundary,
}

/// What the worker publishes to the threads that wait on it.
#[derive(Default)]
struct State {
    /// Turn of the latest start snapshot asked for.
    requested: u64,
    /// Turn of the latest start snapshot taken.
    started: u64,
    /// Turns whose start snapshot was not ready when a change began.
    late: BTreeSet<u64>,
    latest: Option<CheckpointRecord>,
    /// Jobs sent and jobs finished, so a restore can wait for the queue to drain.
    sent: u64,
    done: u64,
    timeline: Timeline,
}

struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Takes snapshots on one background thread, in the order asked.
pub struct Checkpoints {
    sender: Option<Sender<Job>>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    shadow: Arc<Mutex<Shadow>>,
    session: String,
    record: Record,
}

impl Checkpoints {
    pub fn start(shadow: Shadow, session: String, record: Record, notify: Notify) -> Self {
        Self::spawn(shadow, session, record, notify, Duration::ZERO)
    }

    /// Starts a worker that sleeps for `delay` before each snapshot.
    #[cfg(test)]
    pub fn start_with_delay(
        shadow: Shadow,
        session: String,
        record: Record,
        notify: Notify,
        delay: Duration,
    ) -> Self {
        Self::spawn(shadow, session, record, notify, delay)
    }

    fn spawn(
        shadow: Shadow,
        session: String,
        record: Record,
        notify: Notify,
        delay: Duration,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
        });
        let (sender, receiver) = mpsc::channel::<Job>();
        let worker_shared = Arc::clone(&shared);
        let shadow = Arc::new(Mutex::new(shadow));
        let worker_shadow = Arc::clone(&shadow);
        let worker_session = session.clone();
        let worker_record = Arc::clone(&record);
        let worker = std::thread::Builder::new()
            .name("rune-checkpoints".to_owned())
            .spawn(move || {
                let mut reported = false;
                let mut capped = false;
                for job in receiver {
                    if !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                    let shadow = worker_shadow.lock().unwrap_or_else(PoisonError::into_inner);
                    let mut outcome = take(
                        &shadow,
                        &worker_session,
                        &worker_record,
                        &worker_shared,
                        &job,
                    );
                    if outcome.is_ok() && job.boundary == Boundary::End && !capped {
                        capped = true;
                        outcome = shadow.enforce_cap(&worker_session, STORE_CAP);
                    }
                    drop(shadow);
                    {
                        let mut state = worker_shared.lock();
                        state.done = state.done.saturating_add(1);
                    }
                    worker_shared.ready.notify_all();
                    if let Err(error) = outcome
                        && !reported
                    {
                        reported = true;
                        notify(format!("checkpoint failed: {}", error.message()));
                    }
                }
            })
            .ok();
        if worker.is_none() {
            // Without a worker nothing would mark a start snapshot taken.
            shared.lock().started = u64::MAX;
        }
        Self {
            sender: worker.as_ref().map(|_| sender),
            shared,
            worker,
            shadow,
            session,
            record,
        }
    }

    /// Starts from the checkpoints and restores a resumed session already recorded.
    pub fn seed(&self, timeline: Timeline) {
        self.shared.lock().timeline = timeline;
    }

    /// The session's checkpoints and restores so far.
    pub fn timeline(&self) -> Timeline {
        self.shared.lock().timeline.clone()
    }

    /// Blocks until every queued snapshot is written, at most `wait`.
    /// Returns false on timeout.
    pub fn wait_idle(&self, wait: Duration) -> bool {
        let state = self.shared.lock();
        let (state, _) = self
            .shared
            .ready
            .wait_timeout_while(state, wait, |state| state.done < state.sent)
            .unwrap_or_else(PoisonError::into_inner);
        state.done >= state.sent
    }

    /// Waits for queued snapshots, then restores for the request `choose` picks.
    ///
    /// `choose` sees the timeline and returns None when there is nothing to do.
    pub fn restore(
        &self,
        choose: impl FnOnce(&Timeline) -> Result<Option<RestoreRequest>>,
        force: bool,
        dry_run: bool,
    ) -> Result<Option<(RestoreRequest, RestoreReport)>> {
        if !self.wait_idle(RESTORE_WAIT) {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                "a checkpoint is still being written; try again in a moment",
            ));
        }
        let shadow = self.shadow.lock().unwrap_or_else(PoisonError::into_inner);
        let mut timeline = self.timeline();
        let Some(request) = choose(&timeline)? else {
            return Ok(None);
        };
        let report = restore_to(
            &shadow,
            &self.session,
            &mut timeline,
            &request,
            force,
            dry_run,
            &self.record,
        )?;
        self.shared.lock().timeline = timeline;
        Ok(Some((request, report)))
    }

    /// Queues the start snapshot of `turn`.
    pub fn turn_started(&self, turn: u64) {
        self.shared.lock().requested = turn;
        self.send(turn, Boundary::Start);
    }

    /// The turn of the latest start snapshot asked for.
    pub fn turn(&self) -> u64 {
        self.shared.lock().requested
    }

    /// Blocks until the start snapshot of the current turn is written, at most `wait`.
    /// Returns false on timeout.
    pub fn wait_for_start(&self, wait: Duration) -> bool {
        let state = self.shared.lock();
        let (mut state, _) = self
            .shared
            .ready
            .wait_timeout_while(state, wait, |state| state.started < state.requested)
            .unwrap_or_else(PoisonError::into_inner);
        if state.started >= state.requested {
            return true;
        }
        let turn = state.requested;
        state.late.insert(turn);
        false
    }

    /// Queues the end snapshot of `turn`.
    pub fn turn_finished(&self, turn: u64) {
        self.send(turn, Boundary::End);
    }

    /// The latest checkpoint written by this session.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn latest(&self) -> Option<CheckpointRecord> {
        self.shared.lock().latest.clone()
    }

    fn send(&self, turn: u64, boundary: Boundary) {
        if let Some(sender) = &self.sender {
            let mut state = self.shared.lock();
            if sender.send(Job { turn, boundary }).is_ok() {
                state.sent = state.sent.saturating_add(1);
            }
        }
    }
}

impl Drop for Checkpoints {
    /// Waits for the queued snapshots, so the end of the last turn is kept.
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Takes one snapshot and records it, unless it repeats the previous one.
fn take(shadow: &Shadow, session: &str, record: &Record, shared: &Shared, job: &Job) -> Result<()> {
    let staged = shadow.stage();
    let (complete, previous) = {
        let mut state = shared.lock();
        let complete = match job.boundary {
            Boundary::Start => {
                state.started = state.started.max(job.turn);
                let late = state.late.remove(&job.turn);
                shared.ready.notify_all();
                !late
            }
            Boundary::End => true,
        };
        (
            complete,
            state
                .latest
                .as_ref()
                .map(|record| record.snapshot.tree.clone()),
        )
    };
    let (tree, skipped) = staged?;
    if complete && previous.as_deref() == Some(tree.as_str()) {
        return Ok(());
    }
    let side = match job.boundary {
        Boundary::Start => "start",
        Boundary::End => "end",
    };
    let message = format!("turn {} {side}", job.turn);
    let snapshot = shadow.commit(session, &message, tree, skipped)?;
    let checkpoint = CheckpointRecord {
        turn: job.turn,
        boundary: job.boundary,
        snapshot,
        complete,
    };
    let event = checkpoint.event();
    record(event.clone())?;
    let mut state = shared.lock();
    state.timeline.apply(&event);
    state.latest = Some(checkpoint);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::{Utf8Path, Utf8PathBuf};
    use rune_core::paths::Paths;
    use std::time::Instant;

    /// A temporary git repository and a state root outside it.
    struct Fixture {
        _root: tempfile::TempDir,
        workspace: Utf8PathBuf,
        paths: Paths,
    }

    fn git_present() -> bool {
        let present = std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !present {
            eprintln!("skipped: git is not installed");
        }
        present
    }

    /// Runs git in the real repository with a fixed identity.
    fn real_git(workspace: &Utf8Path, args: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(workspace)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().expect("tempdir");
        // On Windows a canonical path has the `\\?\` form, which git refuses;
        // the store and the work tree must still reach git in a form it takes.
        let base = Utf8PathBuf::from_path_buf(root.path().canonicalize().expect("canonical"))
            .expect("utf8");
        let workspace = base.join("work");
        std::fs::create_dir(&workspace).expect("workspace");
        real_git(&workspace, &["init", "-q"]);
        std::fs::write(workspace.join("a.txt"), "a\n").expect("file");
        real_git(&workspace, &["add", "-A"]);
        real_git(&workspace, &["commit", "-qm", "init"]);
        let paths = Paths {
            config_root: base.join("config"),
            state_root: base.join("state"),
            data_root: base.join("data"),
        };
        Fixture {
            _root: root,
            workspace,
            paths,
        }
    }

    fn open(fixture: &Fixture) -> Shadow {
        Shadow::open(&fixture.paths, &fixture.workspace)
            .expect("open")
            .expect("a git workspace has a shadow")
    }

    fn checkpoint(turn: u64, boundary: Boundary, tree: &str) -> SessionEvent {
        SessionEvent::WorkspaceCheckpoint {
            turn,
            boundary,
            tree: tree.to_owned(),
            commit: format!("commit-{tree}"),
            parent: None,
            skipped: Vec::new(),
            complete: true,
        }
    }

    fn restored(turn: u64, direction: RestoreDirection, target: &str, saved: &str) -> SessionEvent {
        SessionEvent::Restored {
            turn,
            direction,
            target: target.to_owned(),
            saved: saved.to_owned(),
        }
    }

    #[test]
    fn timeline_steps_back_over_turns_without_changes() {
        // Turns 1 and 3 changed files; turn 2 recorded nothing because its
        // trees repeated the previous checkpoint.
        let events = vec![
            checkpoint(1, Boundary::Start, "t0"),
            checkpoint(1, Boundary::End, "t1"),
            checkpoint(3, Boundary::End, "t3"),
        ];
        let mut timeline = Timeline::from_events(&events);
        assert_eq!(timeline.undo_target(), Some((3, "t1".to_owned())));
        assert_eq!(
            timeline.before_turn(2).map(|start| start.tree),
            Some("t1".to_owned())
        );
        timeline.apply(&restored(3, RestoreDirection::Undo, "t1", "saved-3"));
        assert_eq!(timeline.undo_target(), Some((1, "t0".to_owned())));
        assert_eq!(timeline.redo_target(), Some((3, "saved-3".to_owned())));
        timeline.apply(&restored(1, RestoreDirection::Undo, "t0", "saved-1"));
        assert_eq!(timeline.undo_target(), None);
        assert_eq!(timeline.redo_target(), Some((1, "saved-1".to_owned())));
        timeline.apply(&restored(1, RestoreDirection::Redo, "saved-1", "saved-r"));
        assert_eq!(timeline.redo_target(), Some((3, "saved-3".to_owned())));
        assert_eq!(timeline.undo_target(), Some((1, "t0".to_owned())));
        assert_eq!(timeline.current(), Some("saved-1"));
    }

    #[test]
    fn a_new_change_clears_redo() {
        let mut timeline = Timeline::from_events(&[
            checkpoint(1, Boundary::Start, "t0"),
            checkpoint(1, Boundary::End, "t1"),
            restored(1, RestoreDirection::Undo, "t0", "saved-1"),
        ]);
        assert!(timeline.redo_target().is_some());
        // A turn that changes nothing keeps the redo target.
        timeline.apply(&checkpoint(2, Boundary::Start, "t0"));
        assert!(timeline.redo_target().is_some());
        timeline.apply(&checkpoint(3, Boundary::End, "t4"));
        assert_eq!(timeline.redo_target(), None);
        assert_eq!(timeline.undo_target(), Some((3, "t0".to_owned())));
    }

    /// The workspace checkpoints a session log holds, in order.
    fn recorded(fixture: &Fixture, id: &rune_core::id::SessionId) -> Vec<SessionEvent> {
        rune_session::store::load_read_only(&fixture.paths.session_dir(id))
            .expect("log")
            .events
            .into_iter()
            .map(|frame| frame.event)
            .filter(|event| matches!(event, SessionEvent::WorkspaceCheckpoint { .. }))
            .collect()
    }

    /// Records checkpoints into a new interactive session log.
    fn recorder_sink(fixture: &Fixture, id: &rune_core::id::SessionId) -> Record {
        let recorder =
            Mutex::new(crate::session_log::Recorder::create(&fixture.paths, id).expect("recorder"));
        Arc::new(move |event| {
            recorder
                .lock()
                .expect("recorder")
                .workspace_checkpoint(event)
        })
    }

    fn worker(fixture: &Fixture, delay: Duration) -> (Checkpoints, rune_core::id::SessionId) {
        let id = rune_core::id::SessionId::generate();
        let notify: Notify = Arc::new(|line| panic!("unexpected notice: {line}"));
        let checkpoints = Checkpoints::start_with_delay(
            open(fixture),
            id.as_str().to_owned(),
            recorder_sink(fixture, &id),
            notify,
            delay,
        );
        (checkpoints, id)
    }

    #[test]
    fn identical_trees_record_one_checkpoint() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let (checkpoints, id) = worker(&fixture, Duration::ZERO);
        checkpoints.turn_started(1);
        assert!(checkpoints.wait_for_start(Duration::from_secs(30)));
        checkpoints.turn_finished(1);
        checkpoints.turn_started(2);
        assert!(checkpoints.wait_for_start(Duration::from_secs(30)));
        std::fs::write(fixture.workspace.join("a.txt"), "changed\n").expect("edit");
        checkpoints.turn_finished(2);
        let latest = loop {
            let latest = checkpoints.latest().expect("a checkpoint");
            if latest.turn == 2 {
                break latest;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(latest.boundary, Boundary::End);
        drop(checkpoints);
        let events = recorded(&fixture, &id);
        let shape: Vec<(u64, Boundary, bool)> = events
            .iter()
            .map(|event| match event {
                SessionEvent::WorkspaceCheckpoint {
                    turn,
                    boundary,
                    complete,
                    ..
                } => (*turn, *boundary, *complete),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            shape,
            vec![(1, Boundary::Start, true), (2, Boundary::End, true)]
        );
        let SessionEvent::WorkspaceCheckpoint { commit, .. } = &events[0] else {
            unreachable!()
        };
        let SessionEvent::WorkspaceCheckpoint { parent, .. } = &events[1] else {
            unreachable!()
        };
        assert_eq!(parent.as_deref(), Some(commit.as_str()));
    }

    #[test]
    fn a_slow_start_snapshot_is_marked_incomplete() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let (checkpoints, id) = worker(&fixture, Duration::from_millis(500));
        checkpoints.turn_started(1);
        let started = Instant::now();
        assert!(!checkpoints.wait_for_start(Duration::from_millis(50)));
        assert!(started.elapsed() < Duration::from_millis(450));
        drop(checkpoints);
        let events = recorded(&fixture, &id);
        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::WorkspaceCheckpoint {
                    turn: 1,
                    boundary: Boundary::Start,
                    complete: false,
                    ..
                }]
            ),
            "{events:?}"
        );
    }

    #[test]
    fn a_failed_snapshot_is_reported_once() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let shadow = open(&fixture);
        std::fs::remove_dir_all(shadow.git_dir()).expect("remove shadow");
        let id = rune_core::id::SessionId::generate();
        let record = recorder_sink(&fixture, &id);
        let notices = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&notices);
        let notify: Notify = Arc::new(move |line| sink.lock().expect("notices").push(line));
        let checkpoints = Checkpoints::start(shadow, id.as_str().to_owned(), record, notify);
        checkpoints.turn_started(1);
        assert!(checkpoints.wait_for_start(Duration::from_secs(30)));
        checkpoints.turn_finished(1);
        drop(checkpoints);
        let notices = notices.lock().expect("notices");
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].starts_with("checkpoint failed: git "),
            "{notices:?}"
        );
        assert!(recorded(&fixture, &id).is_empty());
    }
}
