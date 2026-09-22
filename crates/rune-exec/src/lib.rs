//! Command execution, process supervision, and platform sandboxing.
//!
//! A command is prepared before it runs, so the string that was authorized and
//! the argv that executes can be compared and any drift refused. A prepared
//! command runs in a process group of its own, which is what makes ending it
//! end everything it started.

#![forbid(unsafe_code)]
// Tests assert by panicking. The guards that forbid panicking apply to the
// shipped build, where a panic on user input is a defect.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod command;
pub mod sandbox;
pub mod session;

pub use command::{
    CommandOutcome, DEFAULT_SHELL, Exit, PreparedCommand, TRUNCATION_MARKER, prepare,
    prepare_shell, requires_shell, run, run_with_limits, shell_reason, verify_unchanged,
};
pub use sandbox::{
    LinuxSandbox, MacSandbox, NAMESPACE_HELPER, NullSandbox, SEATBELT_TOOL, Sandbox, SandboxPolicy,
    Support, UNSANDBOXED_OVERRIDE, detect,
};
pub use session::{Buffer, Process, Reading};
