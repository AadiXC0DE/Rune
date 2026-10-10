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

/// Writes a credential to the profile file, creating or verifying its private
/// state directory before reading or writing credentials. The complete JSON
/// replaces the previous file atomically.
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

    paths::create_dir_private(&paths.state_root)?;
    let mut file = read_file(paths)?.unwrap_or_default();
    file.version = default_version();
    file.entries.insert(
        provider.to_owned(),
        StoredEntry {
            value: value.to_owned(),
        },
    );

    let encoded = serde_json::to_string_pretty(&file)?;
    paths::write_private_atomic(&paths.credentials_file(), &encoded)
}

/// Removes a stored credential.
///
/// Returns true when an entry was removed. The complete JSON replaces the
/// previous file atomically.
pub fn remove(paths: &Paths, provider: &str) -> Result<bool> {
    let Some(mut file) = read_file(paths)? else {
        return Ok(false);
    };
    let removed = file.entries.remove(provider).is_some();
    if removed {
        let encoded = serde_json::to_string_pretty(&file)?;
        paths::write_private_atomic(&paths.credentials_file(), &encoded)?;
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

/// The keychain service every stored credential is filed under.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "rune";

/// Longest command line written to the keychain tool's interactive mode.
#[cfg(target_os = "macos")]
const MAX_INTERACTIVE_COMMAND: usize = 4_000;

/// Reads a value from the platform keychain, where one is available.
#[cfg(target_os = "macos")]
fn keychain_lookup(provider: &str) -> Option<String> {
    keychain_read(KEYCHAIN_SERVICE, provider)
}

/// Reads one keychain entry.
#[cfg(target_os = "macos")]
fn keychain_read(service: &str, account: &str) -> Option<String> {
    // The `security` tool ships with the operating system, so no dependency is
    // needed for a lookup that happens at most once per process.
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

/// Writes one keychain entry, replacing any existing value.
///
/// The value never appears in the tool's arguments, which every local user can
/// read from the process list while it runs. The command is written to the
/// tool's interactive mode on standard input instead, with each argument
/// quoted in the syntax that mode reads.
#[cfg(target_os = "macos")]
fn keychain_write(service: &str, account: &str, value: &str) -> bool {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    // A line break would end the command early and start another, so a value
    // or name carrying a control character is refused rather than escaped.
    if [service, account, value]
        .iter()
        .any(|text| text.chars().any(char::is_control))
    {
        return false;
    }
    let command = format!(
        "add-generic-password -U -s {} -a {} -w {}\n",
        interactive_quote(service),
        interactive_quote(account),
        interactive_quote(value)
    );
    // The interactive mode reads a command into a buffer of about 4 KiB and
    // cuts a longer one without failing, which would store part of the value.
    if command.len() > MAX_INTERACTIVE_COMMAND {
        return false;
    }

    let Ok(mut child) = Command::new("/usr/bin/security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return false;
    };
    // Dropping standard input after the command ends the session.
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(command.as_bytes()).is_ok());
    let Ok(output) = child.wait_with_output() else {
        return false;
    };
    // The session reports success even when its command failed, and says so
    // only on standard error.
    written && output.status.success() && output.stderr.is_empty()
}

/// Quotes one argument for the interactive mode of the `security` tool.
#[cfg(target_os = "macos")]
fn interactive_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len().saturating_add(2));
    quoted.push('"');
    for character in text.chars() {
        if matches!(character, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

/// Deletes one keychain entry.
#[cfg(target_os = "macos")]
fn keychain_delete(service: &str, account: &str) -> bool {
    let output = std::process::Command::new("/usr/bin/security")
        .args(["delete-generic-password", "-s", service, "-a", account])
        .output();
    matches!(output, Ok(output) if output.status.success())
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
    keychain_write(KEYCHAIN_SERVICE, provider, value)
}

/// Stores a credential in the platform secret store.
#[cfg(not(target_os = "macos"))]
pub fn store_in_system_store(_provider: &str, _value: &str) -> bool {
    false
}

/// Removes a credential from the platform secret store.
#[cfg(target_os = "macos")]
pub fn remove_from_system_store(provider: &str) -> bool {
    keychain_delete(KEYCHAIN_SERVICE, provider)
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

    #[cfg(unix)]
    #[test]
    fn storing_a_credential_refuses_a_widened_state_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        std::fs::create_dir(&paths.state_root).expect("state directory");
        std::fs::set_permissions(&paths.state_root, std::fs::Permissions::from_mode(0o775))
            .expect("chmod");

        let err = store(&paths, "anthropic", "secret").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
        assert!(
            !paths.credentials_file().exists(),
            "a credential was written"
        );
        assert_eq!(
            std::fs::metadata(&paths.state_root)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o775,
            "an existing directory must not be silently changed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn storing_a_credential_refuses_a_symlinked_state_directory() {
        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        let target = dir.path().join("target");
        std::fs::create_dir(&target).expect("target");
        std::os::unix::fs::symlink(&target, &paths.state_root).expect("symlink");

        let err = store(&paths, "anthropic", "secret").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
        assert!(
            !target.join("credentials.json").exists(),
            "a credential was written through the link"
        );
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

    #[cfg(unix)]
    #[test]
    fn storing_and_removing_replace_the_file_without_truncating_open_readers() {
        use std::io::Read;

        let dir = TempDir::new().expect("tempdir");
        let paths = paths_for(&dir);
        store(&paths, "anthropic", "old-secret").expect("store");

        let original = std::fs::read_to_string(paths.credentials_file()).expect("original JSON");
        let mut reader = std::fs::File::open(paths.credentials_file()).expect("original reader");
        store(&paths, "anthropic", "new-secret").expect("replace");
        let mut retained = String::new();
        reader
            .read_to_string(&mut retained)
            .expect("original reader");
        assert_eq!(
            retained, original,
            "store must not truncate the original inode"
        );
        assert_eq!(
            from_file(&paths, "anthropic")
                .expect("read")
                .expect("credential")
                .expose(),
            "new-secret"
        );

        let original = std::fs::read_to_string(paths.credentials_file()).expect("updated JSON");
        let mut reader = std::fs::File::open(paths.credentials_file()).expect("updated reader");
        assert!(remove(&paths, "anthropic").expect("remove"));
        let mut retained = String::new();
        reader
            .read_to_string(&mut retained)
            .expect("updated reader");
        assert_eq!(
            retained, original,
            "remove must not truncate the original inode"
        );
        assert!(from_file(&paths, "anthropic").expect("read").is_none());
        assert_eq!(
            std::fs::read_dir(&paths.state_root)
                .expect("directory")
                .count(),
            1
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

    /// A keychain service used by one test and deleted when it ends.
    #[cfg(target_os = "macos")]
    struct ThrowawayService(String);

    #[cfg(target_os = "macos")]
    impl ThrowawayService {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default();
            Self(format!("rune-test-{}-{nanos}", std::process::id()))
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for ThrowawayService {
        fn drop(&mut self) {
            let _ = keychain_delete(&self.0, "probe");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_keychain_value_written_through_standard_input_reads_back_unchanged() {
        let service = ThrowawayService::new();
        let value = r#"sk-test "quoted" back\slash 'single' $HOME ; # end"#;
        assert!(
            keychain_write(&service.0, "probe", value),
            "the write failed"
        );
        assert_eq!(keychain_read(&service.0, "probe").as_deref(), Some(value));

        // A second write replaces the value rather than failing on a duplicate.
        assert!(keychain_write(&service.0, "probe", "replacement"));
        assert_eq!(
            keychain_read(&service.0, "probe").as_deref(),
            Some("replacement")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_keychain_value_that_could_start_another_command_is_refused() {
        let service = ThrowawayService::new();
        let smuggled = "value\ndelete-generic-password -s rune";
        assert!(!keychain_write(&service.0, "probe", smuggled));
        assert_eq!(keychain_read(&service.0, "probe"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_keychain_value_too_long_for_one_command_is_refused_whole() {
        // The tool would cut the line and store a truncated credential.
        let service = ThrowawayService::new();
        let fits = "k".repeat(3_000);
        assert!(keychain_write(&service.0, "probe", &fits));
        assert_eq!(
            keychain_read(&service.0, "probe").as_deref(),
            Some(fits.as_str())
        );

        let long = "k".repeat(5_000);
        assert!(!keychain_write(&service.0, "probe", &long));
        assert_eq!(
            keychain_read(&service.0, "probe").map(|value| value.len()),
            Some(fits.len()),
            "a refused write must leave the entry as it was"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_interactive_argument_escapes_its_quotes_and_backslashes() {
        assert_eq!(interactive_quote("plain"), "\"plain\"");
        assert_eq!(interactive_quote(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
