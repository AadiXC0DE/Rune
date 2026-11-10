//! Extra certificate authorities named by `SSL_CERT_FILE`.
//!
//! A corporate TLS proxy or a self-hosted gateway signs with a private
//! authority that the compiled-in roots do not know. Both network clients read
//! the same bundle from here, so a model request and an MCP request trust the
//! same roots. Reading never fails a run: a bundle that cannot be used is
//! reported and the built-in roots stay in place.

use std::io::Read;

/// Largest bundle read.
pub const MAX_BUNDLE_BYTES: u64 = 4 * 1024 * 1024;

/// System bundles tried in order when extra roots replace the built-in ones.
#[cfg(not(windows))]
pub const SYSTEM_BUNDLES: &[&str] = &[
    "/etc/ssl/cert.pem",
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
];

/// System bundles tried in order when extra roots replace the built-in ones.
#[cfg(windows)]
pub const SYSTEM_BUNDLES: &[&str] = &[];

/// Reads the bundle `SSL_CERT_FILE` names.
///
/// `None` when the variable is unset or empty; otherwise the file's bytes or
/// the reason they could not be read.
#[must_use]
pub fn extra_ca_bundle() -> Option<Result<Vec<u8>, String>> {
    extra_ca_bundle_from(std::env::var("SSL_CERT_FILE").ok().as_deref())
}

/// Reads the bundle at `path`, with the same rules as [`extra_ca_bundle`].
#[must_use]
pub fn extra_ca_bundle_from(path: Option<&str>) -> Option<Result<Vec<u8>, String>> {
    let path = path.map(str::trim).filter(|path| !path.is_empty())?;
    Some(read_bundle(path))
}

/// Returns the first readable system bundle and its path.
#[must_use]
pub fn system_bundle() -> Option<(&'static str, Vec<u8>)> {
    SYSTEM_BUNDLES
        .iter()
        .find_map(|path| read_bundle(path).ok().map(|bytes| (*path, bytes)))
}

/// Reads a bundle file, refusing a directory or one over the size limit.
fn read_bundle(path: &str) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|err| describe(&err))?;
    let meta = file.metadata().map_err(|err| describe(&err))?;
    if meta.is_dir() {
        return Err("it is a directory".to_owned());
    }
    if meta.len() > MAX_BUNDLE_BYTES {
        return Err("it is larger than 4 MiB".to_owned());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BUNDLE_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|err| describe(&err))?;
    Ok(bytes)
}

/// A lower-case reason for an I/O failure.
fn describe(err: &std::io::Error) -> String {
    match err.kind() {
        std::io::ErrorKind::NotFound => "no such file".to_owned(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        std::io::ErrorKind::IsADirectory => "it is a directory".to_owned(),
        _ => err.to_string().to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_or_unreadable_bundle_is_reported_not_fatal() {
        assert!(extra_ca_bundle_from(None).is_none());
        assert!(extra_ca_bundle_from(Some("  ")).is_none());

        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing.pem");
        let reason = extra_ca_bundle_from(missing.to_str())
            .expect("set")
            .expect_err("missing");
        assert_eq!(reason, "no such file");

        let reason = extra_ca_bundle_from(dir.path().to_str())
            .expect("set")
            .expect_err("directory");
        assert_eq!(reason, "it is a directory");

        let present = dir.path().join("bundle.pem");
        std::fs::write(&present, b"not a certificate").expect("write");
        let bytes = extra_ca_bundle_from(present.to_str())
            .expect("set")
            .expect("readable");
        assert_eq!(bytes, b"not a certificate");
    }
}
