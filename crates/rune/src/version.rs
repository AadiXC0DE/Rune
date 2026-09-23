//! Version information.

/// The product version.
///
/// This is the single source of truth. The build script and the release
/// pipeline read it from here rather than restating it.
pub const VERSION: &str = "0.1.4";

/// The release channel this build came from.
pub const CHANNEL: &str = "dev";

/// Commit this build was produced from, filled in by the build script.
pub const COMMIT: &str = env!("RUNE_COMMIT");

/// Returns the full version line printed by `rune --version`.
#[must_use]
pub fn version_line() -> String {
    format!("rune {VERSION} ({CHANNEL}, {COMMIT})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_the_program_and_version() {
        let line = version_line();
        assert!(line.starts_with("rune "), "{line}");
        assert!(line.contains(VERSION), "{line}");
    }

    #[test]
    fn version_is_semantic() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "version is not semantic: {VERSION}");
        for part in parts {
            assert!(
                part.parse::<u32>().is_ok(),
                "version component `{part}` is not numeric"
            );
        }
    }
}
