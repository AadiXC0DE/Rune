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
    let mut stack: Vec<&str> = Vec::new();
    for part in &parts {
        match part {
            Component::Root | Component::Current => {}
            Component::Parent => match stack.last() {
                Some(&top) if top != ".." => {
                    stack.pop();
                }
                _ if absolute => {}
                _ => stack.push(".."),
            },
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
