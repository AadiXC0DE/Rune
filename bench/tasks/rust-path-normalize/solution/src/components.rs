//! Splits a path into its parts.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component<'a> {
    /// A leading `/`.
    Root,
    /// A `.` segment.
    Current,
    /// A `..` segment.
    Parent,
    Name(&'a str),
}

pub fn parse(path: &str) -> Vec<Component<'_>> {
    let mut out = Vec::new();
    if path.starts_with('/') {
        out.push(Component::Root);
    }
    for segment in path.split('/') {
        out.push(match segment {
            "" => continue,
            "." => Component::Current,
            ".." => Component::Parent,
            other => Component::Name(other),
        });
    }
    out
}
