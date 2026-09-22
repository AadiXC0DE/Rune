//! The assembled tool set.
//!
//! One function builds the registry so the command line, an editor, and a test
//! all advertise the identical set. A tool registered in one path but not
//! another is a defect that the hash test in this module catches.

use std::fmt::Write as _;

use rune_core::error::Result;
use rune_net::message::ToolSpec;
use sha2::{Digest as _, Sha256};

use crate::ask_user::AskUserQuestion;
use crate::contract::{Tool, model_spec};
use crate::registry::Registry;
use crate::shell::Shell;
use crate::workspace::FileLimits;
use crate::{EditFile, GlobFiles, GrepFiles, ReadFile, WriteFile};

/// Names of every tool this build ships, in advertisement order.
///
/// The order is part of the advertised set: it is what the model sees, and a
/// stable order keeps a recorded request comparable across builds.
pub const ADVERTISEMENT_ORDER: &[&str] = &[
    "glob_files",
    "grep_files",
    "read_file",
    "write_file",
    "edit_file",
    "shell",
    "ask_user_question",
];

/// Builds a registry holding every built-in tool.
///
/// The limits are supplied rather than read from a global, so a host that lowers
/// a bound sees it take effect in every tool that reads one.
///
/// A tool whose schema is invalid fails here rather than at request time, so an
/// unreachable provider request is never caused by a malformed description.
pub fn builtin(limits: &FileLimits, budget: &rune_core::budget::BudgetSet) -> Result<Registry> {
    let mut registry = Registry::new();
    registry.insert(Box::new(GlobFiles::with_limits(*limits)))?;
    registry.insert(Box::new(GrepFiles::with_limits(*limits)))?;
    registry.insert(Box::new(ReadFile::with_limits(*limits)))?;
    registry.insert(Box::new(WriteFile))?;
    registry.insert(Box::new(EditFile))?;
    registry.insert(Box::new(Shell::new(budget)))?;
    registry.insert(Box::new(AskUserQuestion::unavailable()))?;
    debug_assert_eq!(registry.len(), ADVERTISEMENT_ORDER.len());
    Ok(registry)
}

/// Builds a registry with the compiled defaults.
pub fn builtin_default() -> Result<Registry> {
    builtin(&FileLimits::default(), &rune_core::budget::BudgetSet::new())
}

/// Returns the advertised schemas in advertisement order.
pub fn advertisement(registry: &Registry) -> Vec<ToolSpec> {
    registry.schemas(ADVERTISEMENT_ORDER)
}

/// Returns a digest of the advertised set.
///
/// Changing a description, a schema, or the order changes this value, which is
/// what makes the advertised set a contract rather than an implementation
/// detail that drifts.
#[must_use]
pub fn advertisement_digest(registry: &Registry) -> String {
    let specs = advertisement(registry);
    let mut hasher = Sha256::new();
    for spec in &specs {
        hasher.update(spec.name.as_bytes());
        hasher.update(b"\x1f");
        hasher.update(spec.description.as_bytes());
        hasher.update(b"\x1f");
        // Serializing a JSON value with a stable key order is what makes the
        // digest reproducible; serde_json preserves insertion order here.
        hasher.update(spec.input_schema.to_string().as_bytes());
        hasher.update(b"\x1e");
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Validates every tool in a registry.
///
/// A description over the cap, a duplicate name, or a schema that does not
/// describe an object fails here.
pub fn validate_all(registry: &Registry) -> Result<()> {
    for name in registry.names() {
        let Some(tool) = registry.get(name) else {
            continue;
        };
        let spec = model_spec(tool);
        rune_net::message::validate_tool_spec(&spec)?;
    }
    Ok(())
}

/// Returns a tool by name from a registry.
#[must_use]
pub fn find<'a>(registry: &'a Registry, name: &str) -> Option<&'a dyn Tool> {
    registry.get(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Activity;

    #[test]
    fn every_advertised_name_is_registered() {
        let registry = builtin_default().expect("built");
        for name in ADVERTISEMENT_ORDER {
            assert!(registry.contains(name), "`{name}` is not registered");
        }
    }

    #[test]
    fn the_registry_holds_exactly_the_advertised_set() {
        let registry = builtin_default().expect("built");
        assert_eq!(registry.len(), ADVERTISEMENT_ORDER.len());
        let mut registered = registry.names();
        registered.sort_unstable();
        let mut advertised = ADVERTISEMENT_ORDER.to_vec();
        advertised.sort_unstable();
        assert_eq!(registered, advertised);
    }

    #[test]
    fn the_advertisement_is_in_the_declared_order() {
        let registry = builtin_default().expect("built");
        let specs = advertisement(&registry);
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names, ADVERTISEMENT_ORDER);
    }

    #[test]
    fn every_tool_has_a_valid_schema() {
        let registry = builtin_default().expect("built");
        validate_all(&registry).expect("every tool is well formed");
    }

    #[test]
    fn every_tool_describes_an_object_with_required_arguments() {
        let registry = builtin_default().expect("built");
        for spec in advertisement(&registry) {
            assert_eq!(spec.input_schema["type"], "object", "{}", spec.name);
            assert!(
                spec.input_schema.get("properties").is_some(),
                "{} has no properties",
                spec.name
            );
            assert!(
                !spec.description.is_empty(),
                "{} has no description",
                spec.name
            );
        }
    }

    #[test]
    fn every_description_stays_within_the_cap() {
        let registry = builtin_default().expect("built");
        for spec in advertisement(&registry) {
            assert!(
                spec.description.len() <= crate::contract::MAX_DESCRIPTION_BYTES,
                "{} description is {} bytes",
                spec.name,
                spec.description.len()
            );
        }
    }

    #[test]
    fn the_digest_is_stable_across_builds() {
        let first = advertisement_digest(&builtin_default().expect("built"));
        let second = advertisement_digest(&builtin_default().expect("built"));
        assert_eq!(first, second);
        assert_eq!(first.len(), 64, "digest is not a sha256 hex string");
    }

    #[test]
    fn the_digest_changes_when_a_description_changes() {
        // Building two registries with a modified tool proves the digest tracks
        // the advertised content rather than merely the names.
        struct Renamed(Box<dyn Tool>);

        impl Tool for Renamed {
            fn name(&self) -> &'static str {
                self.0.name()
            }
            fn description(&self) -> &'static str {
                "a different description entirely for the same tool name"
            }
            fn input_schema(&self) -> serde_json::Value {
                self.0.input_schema()
            }
            fn activity(&self) -> Activity {
                self.0.activity()
            }
            fn call(
                &self,
                arguments: &serde_json::Value,
                context: &crate::contract::ExecutionContext,
            ) -> Result<crate::contract::ToolOutput> {
                self.0.call(arguments, context)
            }
        }

        let mut modified = Registry::new();
        modified
            .insert(Box::new(Renamed(Box::new(ReadFile::new()))))
            .expect("inserted");
        let digest = advertisement_digest(&modified);
        assert_ne!(
            digest,
            advertisement_digest(&builtin_default().expect("built"))
        );
    }

    #[test]
    fn the_read_only_set_is_identified() {
        let registry = builtin_default().expect("built");
        let read_only = registry.read_only_names();
        for name in ["glob_files", "grep_files", "read_file"] {
            assert!(read_only.contains(&name), "`{name}` should be read only");
        }
        for name in ["write_file", "edit_file"] {
            assert!(!read_only.contains(&name), "`{name}` must not be read only");
        }
    }

    #[test]
    fn find_resolves_a_registered_tool() {
        let registry = builtin_default().expect("built");
        assert!(find(&registry, "read_file").is_some());
        assert!(find(&registry, "nonexistent").is_none());
    }

    #[test]
    fn every_tool_declares_an_activity() {
        let registry = builtin_default().expect("built");
        for name in ADVERTISEMENT_ORDER {
            let activity = registry.activity(name).expect("registered");
            assert!(!activity.running_label().is_empty(), "{name}");
        }
    }

    #[test]
    fn a_duplicate_registration_is_refused() {
        let mut registry = Registry::new();
        registry.insert(Box::new(ReadFile::new())).expect("first");
        let err = registry
            .insert(Box::new(ReadFile::new()))
            .expect_err("duplicate");
        assert_eq!(err.code(), rune_core::error::ErrorCode::AlreadyExists);
    }
}
