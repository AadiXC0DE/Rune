//! Command execution, process supervision, and platform sandboxing.
//!
//! A command is prepared before it runs, so the string that was authorized and
//! the argv that executes can be compared and any drift refused. A prepared
//! command runs in a process group of its own, which is what makes ending it
//! end everything it started.

// Unsafe code is denied rather than forbidden, so that the one call that needs
// it can say so where it is written. Setting a child's resource limits is only
// possible between fork and exec, which is `CommandExt::pre_exec`, and that is
// unsafe by its own contract. Everywhere else in this crate the denial applies.
#![deny(unsafe_code)]
// Tests assert by panicking. The guards that forbid panicking apply to the
// shipped build, where a panic on user input is a defect.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod command;
pub mod sandbox;
pub mod session;

pub use command::{
    CommandOutcome, DEFAULT_ADDRESS_SPACE_BYTES, DEFAULT_CPU_SECONDS, DEFAULT_FILE_BYTES,
    DEFAULT_PROCESSES, DEFAULT_SHELL, Exit, PreparedCommand, ResourceLimits, TRUNCATION_MARKER,
    minimal_environment, prepare, prepare_shell, requires_shell, resource_limits, run,
    run_with_limits, shell_reason, verify_unchanged,
};
pub use sandbox::{
    LinuxSandbox, MacSandbox, NAMESPACE_HELPER, NullSandbox, SEATBELT_TOOL, Sandbox, SandboxPolicy,
    Support, UNSANDBOXED_OVERRIDE, detect,
};
pub use session::{Buffer, Process, Reading};
