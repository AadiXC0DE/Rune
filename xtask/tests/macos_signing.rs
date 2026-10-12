#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

const IDENTITY: &str = "0123456789ABCDEF0123456789ABCDEF01234567";
const IDENTIFIER: &str = "io.github.AadiXC0DE.Rune";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rune-macos-signing-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create fixture");
        Self(path)
    }

    fn stage(&self, binary: &[u8], identity: &str) -> PathBuf {
        let mut tar = vec![0_u8; 512];
        tar[..5].copy_from_slice(b"rune\0");
        tar[100..108].copy_from_slice(b"0000755\0");
        tar[108..116].copy_from_slice(b"0000000\0");
        tar[116..124].copy_from_slice(b"0000000\0");
        tar[124..136].copy_from_slice(format!("{:011o}\0", binary.len()).as_bytes());
        tar[136..148].copy_from_slice(b"00000000000\0");
        tar[148..156].fill(b' ');
        tar[156] = b'0';
        tar[257..263].copy_from_slice(b"ustar\0");
        tar[263..265].copy_from_slice(b"00");
        let checksum: u32 = tar.iter().map(|byte| u32::from(*byte)).sum();
        tar[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        tar.extend_from_slice(binary);
        tar.resize(
            tar.len()
                .div_ceil(512)
                .saturating_mul(512)
                .saturating_add(1024),
            0,
        );
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(9));
        encoder.write_all(&tar).expect("compress fixture");
        let archive = encoder.finish().expect("finish archive");
        std::fs::write(self.0.join("release.tar.gz"), &archive).expect("write archive");
        let manifest = serde_json::json!({
            "target": "aarch64-apple-darwin",
            "archive": "release.tar.gz",
            "bytes": archive.len(),
            "sha256": format!("{:x}", Sha256::digest(&archive)),
            "binary": {
                "name": "rune", "bytes": binary.len(),
                "sha256": format!("{:x}", Sha256::digest(binary)),
            },
            "signature": {"format": "apple-codesign", "identity": identity, "identifier": IDENTIFIER},
        });
        let path = self.0.join("manifest.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");
        path
    }

    fn verify(manifest: &Path, identity: &str, tool_path: Option<&Path>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command.arg("verify-release").arg(manifest).arg(identity);
        if let Some(path) = tool_path {
            command.env("PATH", path);
        }
        command.output().expect("run packaging check")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn packaging_checks_the_published_identity_and_rejects_tampering_even_with_new_checksums() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let tools = fixture.0.join("tools");
    std::fs::create_dir(&tools).expect("create tools");
    let tool = tools.join("codesign");
    // This double asserts the actual verification invocation and extracted
    // bytes. Native cryptographic verification has a separate macOS test.
    std::fs::write(&tool, format!(
        "#!/bin/sh\n[ \"$1\" = --verify ] && [ \"$2\" = --strict ] && [ \"$3\" = --verbose=2 ] && [ \"$4\" = --test-requirement ] || exit 2\n[ \"$5\" = '=certificate leaf = H\"{IDENTITY}\" and identifier \"{IDENTIFIER}\"' ] || exit 3\n[ \"$(/bin/cat \"$6\")\" = 'signed fixture' ] || {{ echo 'invalid signature' >&2; exit 4; }}\n"
    )).expect("write codesign double");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("chmod tool");
    let manifest = fixture.stage(b"signed fixture", IDENTITY);
    let output = Fixture::verify(&manifest, IDENTITY, Some(&tools));
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains(IDENTITY));
    let wrong = "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF";
    let output = Fixture::verify(&manifest, wrong, Some(&tools));
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not match the trusted identity")
    );

    std::fs::write(fixture.0.join("release.tar.gz"), b"corrupted archive")
        .expect("corrupt archive");
    let output = Fixture::verify(&manifest, IDENTITY, Some(&tools));
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("archive checksum or size mismatch"));

    // Recomputing both public hashes must not bypass the signature check.
    let manifest = fixture.stage(b"tampered fixture", IDENTITY);
    let output = Fixture::verify(&manifest, IDENTITY, Some(&tools));
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid signature"));
}

#[test]
fn signing_rejects_ad_hoc_identity_and_non_macos_target_before_building() {
    for (args, message) in [
        (
            vec!["release", "dev", "--sign-identity", "-"],
            "40-digit certificate SHA-1",
        ),
        (
            vec![
                "release",
                "dev",
                "0.0.0",
                "x86_64-unknown-linux-gnu",
                "--sign-identity",
                IDENTITY,
            ],
            "requires a macOS release target",
        ),
    ] {
        let fixture = Fixture::new();
        let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .current_dir(&fixture.0)
            .output()
            .expect("run invalid signing task");
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{output:?}"
        );
        assert!(!fixture.0.join("target").exists());
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires the release certificate; the signed macOS workflow includes this test"]
fn native_codesign_verifies_a_certificate_pinned_archive_and_rejects_modified_code() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let identity = std::env::var("RUNE_SIGNING_IDENTITY")
        .expect("configure the release certificate in the keychain and RUNE_SIGNING_IDENTITY")
        .to_ascii_uppercase();
    std::fs::copy("/usr/bin/true", fixture.0.join("rune")).expect("copy Mach-O fixture");
    std::fs::set_permissions(
        fixture.0.join("rune"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("make fixture writable for signing");
    let output = Command::new("codesign")
        .args([
            "--force",
            "--sign",
            &identity,
            "--timestamp",
            "--options",
            "runtime",
            "--identifier",
            IDENTIFIER,
        ])
        .arg(fixture.0.join("rune"))
        .output()
        .expect("sign native fixture with release certificate");
    assert!(output.status.success(), "{output:?}");
    let mut binary = std::fs::read(fixture.0.join("rune")).expect("read signed code");
    let manifest = fixture.stage(&binary, &identity);
    let output = Fixture::verify(&manifest, &identity, None);
    assert!(output.status.success(), "{output:?}");

    // A valid signature from the wrong certificate is also refused, even when
    // that unexpected identity is substituted in both verification inputs.
    let wrong = "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF";
    let manifest = fixture.stage(&binary, wrong);
    let output = Fixture::verify(&manifest, wrong, None);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("signature check/signing failed"),
        "{output:?}"
    );
    binary[4096] ^= 1;
    let manifest = fixture.stage(&binary, &identity);
    let output = Fixture::verify(&manifest, &identity, None);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("signature check/signing failed"),
        "{output:?}"
    );
}

#[test]
fn signed_release_publishes_verified_identity_and_measures_the_signed_bytes() {
    use std::os::unix::fs::PermissionsExt;

    // Always choose a cross target so the cargo double behaves identically on
    // Linux, Intel Macs and Apple Silicon Macs.
    let target = if env!("TARGET") == "aarch64-apple-darwin" {
        "x86_64-apple-darwin"
    } else {
        "aarch64-apple-darwin"
    };
    for fail_extracted_check in [false, true] {
        let fixture = Fixture::new();
        let tools = fixture.0.join("tools");
        std::fs::create_dir(&tools).expect("tool directory");
        std::fs::write(
            fixture.0.join("Cargo.toml"),
            "[workspace.package]\nversion = \"0.0.0\"\n",
        )
        .expect("fixture version");
        std::fs::write(
            tools.join("cargo"),
            format!(
                r#"#!/bin/sh
[ "$*" = 'build --release --locked -p rune --target {target}' ] || exit 2
mkdir -p target/{target}/release
printf '#!/bin/sh\nexit 0\n' > target/{target}/release/rune
chmod 755 target/{target}/release/rune
"#
            ),
        )
        .expect("cargo double");
        std::fs::write(tools.join("codesign"), format!(r#"#!/bin/sh
if [ "$1" = --force ]; then
    [ "$2" = --sign ] && [ "$3" = '{IDENTITY}' ] && [ "$4" = --timestamp ] && [ "$5" = --options ] && [ "$6" = runtime ] && [ "$7" = --identifier ] && [ "$8" = '{IDENTIFIER}' ] || exit 2
    printf '# signed fixture\n' >> "$9"
    printf 'sign\n' >> "$RUNE_TEST_SIGNING_LOG"
else
    [ "$1" = --verify ] && [ "$2" = --strict ] && [ "$3" = --verbose=2 ] && [ "$4" = --test-requirement ] || exit 3
    [ "$5" = '=certificate leaf = H"{IDENTITY}" and identifier "{IDENTIFIER}"' ] || exit 4
    [ "$(tail -n 1 "$6")" = '# signed fixture' ] || exit 5
    case "$6" in
        */target/{target}/release/rune) printf 'verify-build\n' >> "$RUNE_TEST_SIGNING_LOG" ;;
        *)
            printf 'verify-archive\n' >> "$RUNE_TEST_SIGNING_LOG"
            if [ "$RUNE_TEST_FAIL_EXTRACTED_CHECK" = true ]; then
                echo 'tampered packaged executable' >&2
                exit 6
            fi
            ;;
    esac
fi
"#)).expect("codesign double");
        for name in ["cargo", "codesign"] {
            std::fs::set_permissions(tools.join(name), std::fs::Permissions::from_mode(0o755))
                .expect("tool permissions");
        }
        let path = std::env::join_paths(std::iter::once(tools).chain(std::env::split_paths(
            &std::env::var_os("PATH").expect("PATH"),
        )))
        .expect("tool PATH");
        let log = fixture.0.join("signing.log");
        let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args([
                "release",
                "stable",
                "0.0.0",
                target,
                "--sign-identity",
                &IDENTITY.to_ascii_lowercase(),
            ])
            .current_dir(&fixture.0)
            .env("PATH", path)
            .env_remove("CARGO_TARGET_DIR")
            .env("RUNE_TEST_SIGNING_LOG", &log)
            .env(
                "RUNE_TEST_FAIL_EXTRACTED_CHECK",
                fail_extracted_check.to_string(),
            )
            .output()
            .expect("stage signed fixture");
        let dist = fixture.0.join(format!("target/dist/rune-{target}"));
        assert_eq!(
            std::fs::read_to_string(log).expect("signing log"),
            "sign\nverify-build\nverify-archive\n"
        );
        if fail_extracted_check {
            assert!(!output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("tampered packaged executable")
            );
            assert!(!dist.join("manifest.json").exists());
            assert!(!dist.join(format!("rune-0.0.0-{target}.tar.gz")).exists());
        } else {
            assert!(output.status.success(), "{output:?}");
            let manifest: serde_json::Value = serde_json::from_slice(
                &std::fs::read(dist.join("manifest.json")).expect("published manifest"),
            )
            .expect("manifest JSON");
            let signed = b"#!/bin/sh\nexit 0\n# signed fixture\n";
            assert_eq!(manifest["signature"]["identity"], IDENTITY);
            assert_eq!(manifest["signature"]["identifier"], IDENTIFIER);
            assert_eq!(manifest["binary"]["bytes"], signed.len());
            assert_eq!(
                manifest["binary"]["sha256"],
                format!("{:x}", Sha256::digest(signed))
            );
        }
    }
}
