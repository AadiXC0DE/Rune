//! Resolves `.` and `..` without touching the file system.

use crate::components::{parse, Component};

/// Returns the shortest equivalent form of `path`.
///
/// Absolute paths stay absolute and never go above the root. Relative paths
/// keep the `..` segments that cannot be resolved. An empty result for a
/// relative path is written `.`. No trailing slash is kept, except for `/`.
pub fn normalize(path: &str) -> String {
    let parts = parse(path);
    let absolute = parts.first() == Some(&Component::Root);
    let last = parts.len().saturating_sub(1);
    let mut stack: Vec<&str> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        match part {
            Component::Root => {}
            Component::Current => {
                if i == last {
                    stack.push(".");
                }
            }
            Component::Parent => {
                if stack.pop().is_none() {
                    stack.push("..");
                }
            }
            Component::Name(name) => stack.push(name),
        }
    }
    let joined = stack.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}
