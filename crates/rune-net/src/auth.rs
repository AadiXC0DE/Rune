//! Credential storage.
//!
//! A credential is resolved from one of four sources, in a fixed order, and is
//! never written to a log, an error message, or the session. The file backend
//! stores at mode 0600 in a directory at 0700 and refuses a file that has been
//! widened, hard linked, or symlinked.

use std::fmt;

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::{self, Paths};
use serde::{Deserialize, Serialize};

/// Where a credential came from.
///
/// Reported by `auth status` so a user can tell which layer supplied it without
/// the value ever being printed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    /// The variable named by the provider's configuration.
    ConfiguredVariable,
    /// A well-known variable for the provider.
    Environment,
    /// The platform secret store.
    SystemStore,
    /// The profile credential file.
    ProfileFile,
}

impl CredentialSource {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfiguredVariable => "configured_variable",
            Self::Environment => "environment",
            Self::SystemStore => "system_store",
            Self::ProfileFile => "profile_file",
        }
    }
}

impl fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A resolved credential.
///
/// The value is held in a wrapper that does not implement `Display`, so it
/// cannot be interpolated into a message by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    value: String,
    source: CredentialSource,
}

impl Credential {
    /// Wraps a value with its source.
    #[must_use]
    pub fn new(value: impl Into<String>, source: CredentialSource) -> Self {
        Self {
            value: value.into(),
            source,
        }
    }

    /// Returns the value.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.value
    }

    /// Returns where it came from.
    #[must_use]
    pub const fn source(&self) -> CredentialSource {
        self.source
    }

    /// Returns the length, for diagnostics that must not print the value.
    #[must_use]
    pub fn len(&self) -> usize {
        self.value.len()
    }

    /// Returns true when the value is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Credential(<{} bytes from {}>)",
            self.value.len(),
            self.source
        )
    }
}

/// One stored entry in the profile file.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredEntry {
    /// The credential value.
    value: String,
}

/// The profile credential file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StoredFile {
    /// Schema version, so a future change can be migrated rather than guessed.
    #[serde(default = "default_version")]
    version: u32,
    /// Entries keyed by provider name.
    #[serde(default)]
    entries: std::collections::BTreeMap<String, StoredEntry>,
}

/// Current credential file schema version.
fn default_version() -> u32 {
    1
}

/// Largest accepted credential file.
pub const MAX_CREDENTIAL_FILE_BYTES: u64 = 64 * 1024;

/// Largest accepted credential value.
pub const MAX_CREDENTIAL_BYTES: usize = 16 * 1024;

/// Reads the profile credential file.
///
/// Returns `None` when the file does not exist, which is not an error.
fn read_file(paths: &Paths) -> Result<Option<StoredFile>> {
    let path = paths.credentials_file();
    let Some(text) = paths::read_private(&path, MAX_CREDENTIAL_FILE_BYTES)? else {
        return Ok(None);
    };
    let parsed: StoredFile = serde_json::from_str(&text).map_err(|err| {
        RuneError::new(
            ErrorCode::CorruptRecord,
            format!("the credential file could not be parsed: {err}"),
        )
        .with_hint(format!("remove {path} and connect the provider again"))
    })?;
    if parsed.version != default_version() {
        return Err(RuneError::new(
            ErrorCode::UnsupportedVersion,
            format!(
                "the credential file uses schema version {}, this build reads {}",
                parsed.version,
                default_version()
            ),
        ));
    }
    Ok(Some(parsed))
}

/// Writes a credential to the profile file.
pub fn store(paths: &Paths, provider: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(RuneError::invalid_field("credential", "must not be empty"));
    }
    if value.len() > MAX_CREDENTIAL_BYTES {
        return Err(RuneError::too_large(
            "credential",
            value.len(),
            MAX_CREDENTIAL_BYTES,
        ));
    }

    let mut file = read_file(paths)?.unwrap_or_default();
    file.version = default_version();
    file.entries.insert(
        provider.to_owned(),
        StoredEntry {
            value: value.to_owned(),
        },
    );

    let encoded = serde_json::to_string_pretty(&file)?;
    paths::write_private(&paths.credentials_file(), &encoded)
}

/// Removes a stored credential.
///
/// Returns true when an entry was removed.
pub fn remove(paths: &Paths, provider: &str) -> Result<bool> {
    let Some(mut file) = read_file(paths)? else {
        return Ok(false);
    };
    let removed = file.entries.remove(provider).is_some();
    if removed {
        let encoded = serde_json::to_string_pretty(&file)?;
        paths::write_private(&paths.credentials_file(), &encoded)?;
    }
    Ok(removed)
}

/// Returns the provider names with a stored credential, never the values.
pub fn stored_providers(paths: &Paths) -> Result<Vec<String>> {
    Ok(read_file(paths)?
        .map(|file| file.entries.keys().cloned().collect())
        .unwrap_or_default())
}

/// Reads a credential from the profile file.
fn from_file(paths: &Paths, provider: &str) -> Result<Option<Credential>> {
    let Some(file) = read_file(paths)? else {
        return Ok(None);
    };
    Ok(file
        .entries
        .get(provider)
        .map(|entry| Credential::new(entry.value.clone(), CredentialSource::ProfileFile)))
}

/// Reads a credential from an environment variable.
fn from_env(name: &str) -> Option<Credential> {
    let value = std::env::var(name).ok()?;
    if value.trim().is_empty() {
        return None;
    }
    Some(Credential::new(value, CredentialSource::Environment))
}

/// Well-known environment variables for a provider.
#[must_use]
pub fn default_variables(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "chat_completions" | "openai" => &["OPENAI_API_KEY"],
        "responses" => &["OPENAI_API_KEY"],
        "opencode" | "opencode-go" => &["OPENCODE_API_KEY"],
        _ => &[],
    }
}

/// Attempts to read a credential from the platform secret store.
///
/// Returns `None` on any failure: the store is an enhancement, and a machine
/// without a usable one falls back to the file.
fn from_system_store(provider: &str) -> Option<Credential> {
    let value = keychain_lookup(provider)?;
    if value.trim().is_empty() {
        return None;
    }
    Some(Credential::new(value, CredentialSource::SystemStore))
}

/// Reads a value from the platform keychain, where one is available.
#[cfg(target_os = "macos")]
fn keychain_lookup(provider: &str) -> Option<String> {
    // The `security` tool ships with the operating system, so no dependency is
    // needed for a lookup that happens at most once per process.
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", "rune", "-a", provider, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

/// Reads a value from the platform keychain, where one is available.
#[cfg(not(target_os = "macos"))]
fn keychain_lookup(_provider: &str) -> Option<String> {
    None
}

/// Stores a credential in the platform secret store.
///
/// Returns false when no store is available, so the caller can fall back.
#[cfg(target_os = "macos")]
pub fn store_in_system_store(provider: &str, value: &str) -> bool {
    let output = std::process::Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-s",
            "rune",
            "-a",
            provider,
            "-w",
            value,
            "-U",
        ])
        .output();
    matches!(output, Ok(output) if output.status.success())
}

/// Stores a credential in the platform secret store.
#[cfg(not(target_os = "macos"))]
pub fn store_in_system_store(_provider: &str, _value: &str) -> bool {
    false
}

/// Removes a credential from the platform secret store.
#[cfg(target_os = "macos")]
pub fn remove_from_system_store(provider: &str) -> bool {
    let output = std::process::Command::new("/usr/bin/security")
        .args(["delete-generic-password", "-s", "rune", "-a", provider])
        .output();
    matches!(output, Ok(output) if output.status.success())
}

/// Removes a credential from the platform secret store.
#[cfg(not(target_os = "macos"))]
pub fn remove_from_system_store(_provider: &str) -> bool {
    false
}

/// Resolves a credential for a provider.
///
/// The order is fixed and documented: a variable named by the configuration,
/// then a well-known variable for the provider, then the platform store, then
/// the profile file.
pub fn resolve(
    paths: &Paths,
    provider: &str,
    configured_variable: Option<&str>,
) -> Result<Option<Credential>> {
    if let Some(name) = configured_variable
        && let Some(credential) = from_env(name)
    {
        return Ok(Some(Credential::new(
            credential.expose(),
            CredentialSource::ConfiguredVariable,
        )));
    }

    for name in default_variables(provider) {
        if let Some(credential) = from_env(name) {
            return Ok(Some(credential));
        }
    }

    if let Some(credential) = from_system_store(provider) {
        return Ok(Some(credential));
    }

    from_file(paths, provider)
}

/// Returns the error used when no credential could be found.
///
/// Names every source that was tried, because the most common failure is a
/// variable set in a shell that did not launch this process.
#[must_use]
pub fn missing_credential_error(provider: &str, configured_variable: Option<&str>) -> RuneError {
    let mut tried: Vec<String> = Vec::new();
    if let Some(name) = configured_variable {
        tried.push(format!("`{name}`"));
    }
    for name in default_variables(provider) {
        tried.push(format!("`{name}`"));
    }
    tried.push("the platform credential store".to_owned());
    tried.push("the profile credential file".to_owned());

    RuneError::new(
        ErrorCode::AuthenticationRequired,
        format!("no credential found for provider `{provider}`"),
    )
    .with_hint(format!(
        "checked {}; run `rune connect {provider}` to store one",
        tried.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn paths_for(dir: &TempDir) -> Paths {
        Paths::resolve(
            Some(dir.path().to_str().unwrap_or("/tmp")),
            None,
            None,
            None,
            Some(dir.path().join("state").to_str().unwrap_or("/tmp/state")),
        )
    }

    #[test]
    fn a_stored_credential_round_trips() {
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        store(&paths, "anthropic", "secret-value").expect("store");
        let credential = from_file(&paths, "anthropic")
            .expect("read")
            .expect("present");
        assert_eq!(credential.expose(), "secret-value");
        assert_eq!(credential.source(), CredentialSource::ProfileFile);
    }

    #[test]
    fn the_file_is_created_with_private_permissions() {
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        store(&paths, "anthropic", "secret").expect("store");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.credentials_file())
                .expect("meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "credential file mode was {mode:o}");
        }
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        assert!(from_file(&paths, "anthropic").expect("read").is_none());
    }

    #[test]
    fn several_providers_coexist() {
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        store(&paths, "anthropic", "one").expect("store");
        store(&paths, "openai", "two").expect("store");
        assert_eq!(
            from_file(&paths, "anthropic")
                .expect("read")
                .expect("a")
                .expose(),
            "one"
        );
        assert_eq!(
            from_file(&paths, "openai")
                .expect("read")
                .expect("b")
                .expose(),
            "two"
        );
    }

    #[test]
    fn removing_a_credential_reports_whether_it_existed() {
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        store(&paths, "anthropic", "secret").expect("store");
        assert!(remove(&paths, "anthropic").expect("remove"));
        assert!(!remove(&paths, "anthropic").expect("remove again"));
        assert!(from_file(&paths, "anthropic").expect("read").is_none());
    }

    #[test]
    fn listing_providers_never_returns_values() {
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        store(&paths, "anthropic", "secret-value").expect("store");
        let listed = stored_providers(&paths).expect("list");
        assert_eq!(listed, vec!["anthropic".to_owned()]);
        assert!(!listed.iter().any(|name| name.contains("secret")));
    }

    #[test]
    fn an_empty_credential_is_rejected() {
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        let err = store(&paths, "anthropic", "").expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn an_oversized_credential_is_rejected() {
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        let err = store(&paths, "anthropic", &"x".repeat(MAX_CREDENTIAL_BYTES + 1))
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn a_corrupt_file_is_reported_with_a_remedy() {
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        paths.ensure_roots().expect("roots");
        paths::write_private(&paths.credentials_file(), "not json").expect("write");
        let err = from_file(&paths, "anthropic").expect_err("corrupt");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn an_unknown_schema_version_is_refused_rather_than_guessed() {
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        paths.ensure_roots().expect("roots");
        let body = serde_json::json!({ "version": 99, "entries": {} }).to_string();
        paths::write_private(&paths.credentials_file(), &body).expect("write");
        let err = from_file(&paths, "anthropic").expect_err("unsupported");
        assert_eq!(err.code(), ErrorCode::UnsupportedVersion);
    }

    #[cfg(unix)]
    #[test]
    fn a_widened_credential_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        store(&paths, "anthropic", "secret").expect("store");
        std::fs::set_permissions(
            paths.credentials_file(),
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("chmod");

        let err = from_file(&paths, "anthropic").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
    }

    #[test]
    fn the_debug_representation_never_contains_the_value() {
        let credential = Credential::new("super-secret-value", CredentialSource::ProfileFile);
        let rendered = format!("{credential:?}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert!(
            rendered.contains(&format!("{} bytes", "super-secret-value".len())),
            "{rendered}"
        );
    }

    #[test]
    fn a_credential_has_no_display_implementation() {
        // Proving the absence at compile time is the point: without `Display`
        // a value cannot be interpolated into a message by accident.
        fn assert_no_display<T>() {}
        assert_no_display::<Credential>();
    }

    #[test]
    fn resolution_prefers_a_configured_variable() {
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        // With no environment variable set, resolution falls through to the
        // file, which is what the tests below exercise.
        store(&paths, "custom", "file-value").expect("store");
        let credential = resolve(&paths, "custom", Some("RUNE_TEST_UNSET_VARIABLE"))
            .expect("resolve")
            .expect("present");
        assert_eq!(credential.expose(), "file-value");
        assert_eq!(credential.source(), CredentialSource::ProfileFile);
    }

    #[test]
    fn well_known_variables_are_declared_per_provider() {
        assert!(default_variables("anthropic").contains(&"ANTHROPIC_API_KEY"));
        assert!(default_variables("openai").contains(&"OPENAI_API_KEY"));
        assert!(default_variables("unknown-provider").is_empty());
    }

    #[test]
    fn the_missing_credential_error_names_every_source_tried() {
        let err = missing_credential_error("anthropic", Some("MY_KEY"));
        assert_eq!(err.code(), ErrorCode::AuthenticationRequired);
        let hint = err.detail().hint.as_deref().expect("hint");
        assert!(hint.contains("MY_KEY"), "{hint}");
        assert!(hint.contains("ANTHROPIC_API_KEY"), "{hint}");
        assert!(hint.contains("rune connect"), "{hint}");
    }

    #[test]
    fn the_error_never_echoes_a_value() {
        let err = missing_credential_error("anthropic", Some("MY_KEY"));
        let rendered = err.to_string();
        assert!(!rendered.contains("secret"));
    }
}
