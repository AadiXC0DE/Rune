//! Finding and installing a newer published release.
//!
//! The release document names the archives; the release's `SHA256SUMS` is the
//! digest every archive is checked against before anything is written.

use std::time::Duration;

use camino::Utf8Path;
use rune_core::error::{ErrorCode, Result, RuneError};

/// The latest release of the repository, as the GitHub API describes it.
const RELEASES_API: &str = "https://api.github.com/repos/AadiXC0DE/Rune/releases/latest";

/// How long the archive and its checksums may take to download.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// This build's target triple, which names its archive.
const TARGET: &str = env!("RUNE_TARGET");

/// One published release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The version, without a leading `v`.
    pub version: String,
    /// Each asset's name and download address.
    pub assets: Vec<(String, String)>,
}

/// What an install replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    /// The version installed.
    pub version: String,
    /// The target triple of the archive.
    pub target: &'static str,
}

/// The release document's address. Debug builds read `RUNE_RELEASES_API`, so a
/// test can serve a release of its own.
fn releases_api() -> String {
    #[cfg(debug_assertions)]
    if let Some(url) = std::env::var("RUNE_RELEASES_API")
        .ok()
        .filter(|url| !url.is_empty())
    {
        return url;
    }
    RELEASES_API.to_owned()
}

/// Reads the latest published release.
pub fn latest_release(timeout: Duration) -> Result<Release> {
    let fetched =
        rune_net::transport::fetch_url(&releases_api(), "application/vnd.github+json", timeout)
            .map_err(|err| {
                RuneError::new(
                    ErrorCode::TransportFailure,
                    format!("could not check for a newer release: {}", err.message()),
                )
            })?;
    match fetched.status {
        200 => parse_release(&fetched.body),
        403 | 429 => Err(RuneError::new(
            ErrorCode::RateLimited,
            "could not check for a newer release: GitHub rate limit; try again later",
        )),
        status => Err(RuneError::new(
            ErrorCode::TransportFailure,
            format!("could not check for a newer release: the release lookup answered {status}"),
        )),
    }
}

/// Reads a release document.
fn parse_release(body: &[u8]) -> Result<Release> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|err| {
        RuneError::new(
            ErrorCode::ProtocolViolation,
            format!("the release document is not JSON: {err}"),
        )
    })?;
    let version = value["tag_name"]
        .as_str()
        .map(|tag| tag.trim_start_matches('v').to_owned())
        .filter(|tag| !tag.is_empty())
        .ok_or_else(|| {
            RuneError::new(
                ErrorCode::ProtocolViolation,
                "the release document names no tag",
            )
        })?;
    let assets = value["assets"]
        .as_array()
        .map(|assets| {
            assets
                .iter()
                .filter_map(|asset| {
                    Some((
                        asset["name"].as_str()?.to_owned(),
                        asset["browser_download_url"].as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { version, assets })
}

/// The archive name for this build's target.
#[must_use]
pub fn asset_name(version: &str) -> String {
    asset_name_for(version, TARGET)
}

fn asset_name_for(version: &str, target: &str) -> String {
    let extension = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("rune-{version}-{target}.{extension}")
}

/// Whether `candidate` is a later version than `current`, comparing the three
/// numeric components. A version that does not parse is never later.
#[must_use]
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |version: &str| -> Option<(u64, u64, u64)> {
        let core = version.trim_start_matches('v').split(['-', '+']).next()?;
        let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
        Some((parts.next()??, parts.next()??, parts.next()??))
    };
    match (parse(candidate), parse(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// The command that upgrades a binary a package manager installed, when one did.
#[must_use]
pub fn managed_by(path: &Utf8Path) -> Option<&'static str> {
    let text = path.as_str().replace('\\', "/").to_ascii_lowercase();
    if text.contains("/cellar/") || text.contains("/homebrew/") {
        Some("this rune was installed by Homebrew; run brew upgrade rune")
    } else if text.contains("/scoop/apps/") {
        Some("this rune was installed by Scoop; run scoop update rune")
    } else if text.contains("rune-npm") {
        Some("this rune was installed by npm; run npm update -g @rune-sh/cli")
    } else {
        None
    }
}

/// Downloads `name` from the release.
fn download(release: &Release, name: &str) -> Result<Vec<u8>> {
    let url = release
        .assets
        .iter()
        .find(|(asset, _)| asset == name)
        .map(|(_, url)| url)
        .ok_or_else(|| {
            RuneError::new(
                ErrorCode::NotFound,
                format!("release {} has no {name}", release.version),
            )
        })?;
    let fetched = rune_net::transport::fetch_url(url, "application/octet-stream", DOWNLOAD_TIMEOUT)
        .map_err(|err| {
            RuneError::new(
                ErrorCode::TransportFailure,
                format!("could not download {name}: {}", err.message()),
            )
        })?;
    if fetched.status != 200 {
        return Err(RuneError::new(
            ErrorCode::TransportFailure,
            format!(
                "could not download {name}: the server answered {}",
                fetched.status
            ),
        ));
    }
    Ok(fetched.body)
}

/// The digest `SHA256SUMS` lists for `name`.
fn listed_digest(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        let listed = fields.next()?.trim_start_matches('*');
        (listed == name && fields.next().is_none()).then(|| digest.to_ascii_lowercase())
    })
}

/// Downloads, verifies and installs `release` over `target`.
pub fn install_release(release: &Release, target: &Utf8Path) -> Result<Installed> {
    let name = asset_name(&release.version);
    if !release.assets.iter().any(|(asset, _)| *asset == name) {
        return Err(RuneError::new(
            ErrorCode::NotFound,
            format!("release {} has no archive for {TARGET}", release.version),
        ));
    }
    let bytes = download(release, &name)?;
    let sums = String::from_utf8_lossy(&download(release, "SHA256SUMS")?).into_owned();
    let digest = listed_digest(&sums, &name).ok_or_else(|| {
        RuneError::new(
            ErrorCode::InvalidState,
            format!("SHA256SUMS has no line for {name}; nothing was installed"),
        )
    })?;
    let artifact = crate::install::Artifact::new(bytes);
    artifact.verify(&digest).map_err(|err| {
        RuneError::new(
            ErrorCode::InvalidState,
            format!(
                "{name} does not match SHA256SUMS; nothing was installed ({})",
                err.message()
            ),
        )
    })?;
    #[cfg(target_os = "macos")]
    check_signature(&artifact)?;
    crate::install::install(target, &artifact, &digest)?;
    Ok(Installed {
        version: release.version.clone(),
        target: TARGET,
    })
}

/// A signed binary must verify before it replaces the running one.
#[cfg(target_os = "macos")]
fn check_signature(artifact: &crate::install::Artifact) -> Result<()> {
    let binary = crate::install::release_binary(&artifact.bytes)?;
    let staged = std::env::temp_dir().join(format!("rune-upgrade-{}", std::process::id()));
    std::fs::write(&staged, &binary)?;
    let codesign = |arguments: &[&str]| {
        std::process::Command::new("codesign")
            .args(arguments)
            .arg(&staged)
            .output()
            .is_ok_and(|output| output.status.success())
    };
    let signed = codesign(&["-dv"]);
    let verified = !signed || codesign(&["--verify", "--strict"]);
    let _ = std::fs::remove_file(&staged);
    if verified {
        Ok(())
    } else {
        Err(RuneError::new(
            ErrorCode::InvalidState,
            "the downloaded binary's signature does not verify; nothing was installed",
        ))
    }
}

/// How long a recorded check stands before the next one.
const CHECK_INTERVAL: Duration = Duration::from_hours(24);

/// How long the background check may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// The last check, kept in `<state root>/update-check.json`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct CheckRecord {
    /// Unix seconds.
    checked_at: u64,
    /// The latest version then.
    latest: String,
}

fn check_file(paths: &rune_core::paths::Paths) -> camino::Utf8PathBuf {
    paths.state_root.join("update-check.json")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The recorded check; an unreadable file counts as none.
fn recorded(paths: &rune_core::paths::Paths) -> Option<CheckRecord> {
    let text = std::fs::read_to_string(check_file(paths)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The latest version the last check found, whatever its age.
#[must_use]
pub fn recorded_latest(paths: &rune_core::paths::Paths) -> Option<String> {
    recorded(paths).map(|record| record.latest)
}

/// The notice for `latest`, when it is newer than this build.
#[must_use]
pub fn notice(latest: &str) -> Option<String> {
    is_newer(latest, crate::version::VERSION)
        .then(|| format!("rune {latest} is available; run rune upgrade"))
}

/// Starts a check on another thread when the recorded one is missing or a day
/// old. The thread records what it finds and returns the latest version.
#[must_use]
pub fn start_check(
    paths: &rune_core::paths::Paths,
) -> Option<std::thread::JoinHandle<Option<String>>> {
    let now = unix_now();
    if recorded(paths)
        .is_some_and(|record| now.saturating_sub(record.checked_at) < CHECK_INTERVAL.as_secs())
    {
        return None;
    }
    let file = check_file(paths);
    std::thread::Builder::new()
        .name("update-check".to_owned())
        .spawn(move || {
            let release = latest_release(CHECK_TIMEOUT).ok()?;
            let record = CheckRecord {
                checked_at: unix_now(),
                latest: release.version.clone(),
            };
            if let Ok(text) = serde_json::to_string(&record) {
                let _ = rune_core::paths::write_private_atomic(&file, &text);
            }
            Some(release.version)
        })
        .ok()
}

/// Installs the latest release over the running binary after a session, when
/// the last check found one newer. Returns the line to print.
#[must_use]
pub fn install_after_session(paths: &rune_core::paths::Paths) -> Option<String> {
    let latest = recorded_latest(paths)?;
    if !is_newer(&latest, crate::version::VERSION) {
        return None;
    }
    let outcome = crate::install::current_binary().and_then(|target| {
        if let Some(refusal) = managed_by(&target) {
            return Err(RuneError::new(ErrorCode::Unsupported, refusal));
        }
        let release = latest_release(CHECK_TIMEOUT)?;
        install_release(&release, &target)
    });
    Some(match outcome {
        Ok(installed) => format!(
            "upgraded rune {} to {} ({})",
            crate::version::VERSION,
            installed.version,
            installed.target
        ),
        Err(err) => format!("upgrade failed: {}", err.message()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_latest_release_document() {
        let body = br#"{"tag_name": "v1.2.3", "assets": [
            {"name": "rune-1.2.3-aarch64-apple-darwin.tar.gz", "browser_download_url": "https://x/a.tar.gz"},
            {"name": "SHA256SUMS", "browser_download_url": "https://x/SHA256SUMS"},
            {"name": "broken"}
        ]}"#;
        let release = parse_release(body).expect("release");
        assert_eq!(release.version, "1.2.3");
        assert_eq!(
            release.assets,
            vec![
                (
                    "rune-1.2.3-aarch64-apple-darwin.tar.gz".to_owned(),
                    "https://x/a.tar.gz".to_owned()
                ),
                ("SHA256SUMS".to_owned(), "https://x/SHA256SUMS".to_owned()),
            ]
        );
        assert!(parse_release(b"{}").is_err());
        assert!(parse_release(b"not json").is_err());
    }

    #[test]
    fn asset_names_follow_the_target() {
        assert_eq!(
            asset_name_for("1.2.3", "aarch64-apple-darwin"),
            "rune-1.2.3-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_name_for("1.2.3", "x86_64-pc-windows-msvc"),
            "rune-1.2.3-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            asset_name("0.9.0"),
            format!(
                "rune-0.9.0-{TARGET}.{}",
                if TARGET.contains("windows") {
                    "zip"
                } else {
                    "tar.gz"
                }
            )
        );
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("0.9.9", "0.10.0"));
        assert!(!is_newer("0.1.17", "0.1.17"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(is_newer("0.2.0-rc.1", "0.1.0"));
    }

    #[test]
    fn package_manager_installs_are_refused() {
        assert_eq!(
            managed_by(Utf8Path::new("/opt/homebrew/Cellar/rune/0.1.0/bin/rune")),
            Some("this rune was installed by Homebrew; run brew upgrade rune")
        );
        assert_eq!(
            managed_by(Utf8Path::new(
                "C:\\Users\\ada\\scoop\\apps\\rune\\current\\rune.exe"
            )),
            Some("this rune was installed by Scoop; run scoop update rune")
        );
        assert_eq!(
            managed_by(Utf8Path::new("/home/ada/.cache/rune-npm/0.1.0/rune")),
            Some("this rune was installed by npm; run npm update -g @rune-sh/cli")
        );
        assert_eq!(managed_by(Utf8Path::new("/home/ada/.local/bin/rune")), None);
    }

    #[test]
    fn a_fresh_record_skips_the_check_and_names_the_notice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let paths = rune_core::paths::Paths::resolve(None, None, None, None, Some(root.as_str()));
        assert_eq!(recorded_latest(&paths), None);
        let record = CheckRecord {
            checked_at: unix_now(),
            latest: "99.0.0".to_owned(),
        };
        std::fs::write(
            check_file(&paths),
            serde_json::to_string(&record).expect("json"),
        )
        .expect("write");
        assert!(
            start_check(&paths).is_none(),
            "a fresh record needs no check"
        );
        assert_eq!(recorded_latest(&paths).as_deref(), Some("99.0.0"));
        assert_eq!(
            notice("99.0.0").as_deref(),
            Some("rune 99.0.0 is available; run rune upgrade")
        );
        assert_eq!(notice(crate::version::VERSION), None);
        std::fs::write(check_file(&paths), "not json").expect("write");
        assert_eq!(recorded_latest(&paths), None, "an unreadable file is none");
    }

    #[test]
    fn the_listed_digest_is_found_by_exact_name() {
        let sums =
            "aa  rune-1.2.3-a.tar.gz\nBB *rune-1.2.3-b.tar.gz\ncc  rune-1.2.3-a.tar.gz.sha256\n";
        assert_eq!(
            listed_digest(sums, "rune-1.2.3-a.tar.gz").as_deref(),
            Some("aa")
        );
        assert_eq!(
            listed_digest(sums, "rune-1.2.3-b.tar.gz").as_deref(),
            Some("bb")
        );
        assert_eq!(listed_digest(sums, "rune-1.2.3-c.tar.gz"), None);
    }
}
