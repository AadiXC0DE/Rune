//! Built-in tools and the tool registry.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod contract;
pub mod edit_file;
pub mod glob_files;
pub mod grep_files;
pub mod inventory;
pub mod mutation;
pub mod read_file;
pub mod registry;
pub mod result_store;
pub mod workspace;
pub mod write_file;

pub use contract::{Activity, Cancellation, ExecutionContext, Tool, ToolOutput};
pub use edit_file::EditFile;
pub use glob_files::GlobFiles;
pub use grep_files::GrepFiles;
pub use mutation::{Applied, ChangeSpan, Occurrence, Prepared, Preview};
pub use read_file::ReadFile;
pub use registry::Registry;
pub use result_store::{Handle, Preview as ResultPreview, Store};
pub use workspace::{FileLimits, ResolvedPath, Walker, resolve};
pub use write_file::WriteFile;
