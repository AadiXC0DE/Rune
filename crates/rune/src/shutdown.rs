//! Turning a termination signal into a clean stop.
//!
//! A benchmark runner ends a harness with SIGTERM when its time is up, and a
//! user with Ctrl-C. Either should stop the run the way its own limits do:
//! cancel the turn, end every command it started, record the session and
//! report what it did, rather than die and leave commands running.

use rune_core::error::{ErrorCode, Result, RuneError};

/// Turns the first termination signal into a cancellation and remembers which
/// it was.
pub struct Shutdown {
    signal: std::sync::Arc<std::sync::atomic::AtomicI32>,
    #[cfg(unix)]
    handle: signal_hook::iterator::Handle,
    #[cfg(unix)]
    thread: Option<std::thread::JoinHandle<()>>,
}

/// SIGINT, which exits 130.
pub const SIGINT: i32 = 2;
/// SIGHUP, which exits 143 like SIGTERM.
pub const SIGHUP: i32 = 1;
/// SIGTERM, which exits 143.
pub const SIGTERM: i32 = 15;

/// The exit code a run stopped by `signal` ends with.
#[must_use]
pub const fn exit_code(signal: i32) -> i32 {
    if signal == SIGINT { 130 } else { 143 }
}

/// The signal's name, for the error that reports it.
#[must_use]
pub const fn name(signal: i32) -> &'static str {
    match signal {
        SIGINT => "SIGINT",
        SIGHUP => "SIGHUP",
        _ => "SIGTERM",
    }
}

/// The error a run stopped by `signal` reports.
#[must_use]
pub fn stopped_by(signal: i32) -> RuneError {
    RuneError::new(
        ErrorCode::Cancelled,
        format!("the run was stopped by {}", name(signal)),
    )
}

impl Shutdown {
    /// Installs handlers for SIGINT, SIGTERM and SIGHUP. `on_signal` runs
    /// once, on a helper thread, for the first one. A second signal exits at
    /// once with the first one's code: the user insisted, so cleanup is
    /// skipped.
    #[cfg(unix)]
    pub fn install(on_signal: impl Fn() + Send + 'static) -> Result<Self> {
        use std::sync::atomic::Ordering;
        let mut signals =
            signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP]).map_err(|err| {
                RuneError::new(
                    ErrorCode::IoFailure,
                    format!("could not install signal handlers: {err}"),
                )
            })?;
        let handle = signals.handle();
        let signal = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let seen = std::sync::Arc::clone(&signal);
        let thread = std::thread::Builder::new()
            .name("rune-signals".to_owned())
            .spawn(move || {
                for received in signals.forever() {
                    let first = seen.load(Ordering::SeqCst);
                    if first != 0 {
                        std::process::exit(exit_code(first));
                    }
                    seen.store(received, Ordering::SeqCst);
                    on_signal();
                }
            })
            .map_err(|err| RuneError::new(ErrorCode::IoFailure, err.to_string()))?;
        Ok(Self {
            signal,
            handle,
            thread: Some(thread),
        })
    }

    /// Without unix signals there is nothing to install; nothing is received.
    #[cfg(not(unix))]
    pub fn install(_on_signal: impl Fn() + Send + 'static) -> Result<Self> {
        Ok(Self {
            signal: std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0)),
        })
    }

    /// The signal received, if any.
    #[must_use]
    pub fn received(&self) -> Option<i32> {
        Some(self.signal.load(std::sync::atomic::Ordering::SeqCst)).filter(|signal| *signal != 0)
    }
}

impl Drop for Shutdown {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // Closing the iterator ends the helper thread.
            self.handle.close();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_map_to_their_exit_codes_and_names() {
        assert_eq!(exit_code(SIGINT), 130);
        assert_eq!(exit_code(SIGTERM), 143);
        assert_eq!(exit_code(SIGHUP), 143);
        assert_eq!(name(SIGHUP), "SIGHUP");
        assert_eq!(
            stopped_by(SIGTERM).message(),
            "the run was stopped by SIGTERM"
        );
        assert_eq!(stopped_by(SIGINT).code(), ErrorCode::Cancelled);
    }
}
