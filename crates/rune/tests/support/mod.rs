//! Child environments shared by the Rust and embedded Python fixtures.

// Each integration test is a separate crate and uses only part of this module.
#![allow(dead_code, clippy::expect_used)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::process::Command;

/// Preserve only the OS dependencies of an otherwise stripped environment.
///
/// Winsock expands variables in provider DLL paths before loading them:
/// https://learn.microsoft.com/en-us/windows/win32/api/ws2spi/nf-ws2spi-wscgetproviderpath
/// The combined 32/64-bit provider API also documents `%windir%` paths:
/// https://learn.microsoft.com/en-us/windows/win32/api/ws2spi/nf-ws2spi-wscinstallproviderandchains64_32
/// These absolute paths need no PATH lookup. Fixtures that run external programs
/// opt in with `with_external_programs`; do not inherit Rune settings,
/// credentials, proxies, or user configuration directories here.
fn os_environment() -> BTreeMap<&'static str, OsString> {
    #[cfg(windows)]
    {
        let system_root = std::env::var_os("SystemRoot").expect("Windows SystemRoot");
        let windir = std::env::var_os("windir").unwrap_or_else(|| system_root.clone());
        BTreeMap::from([("SystemRoot", system_root), ("windir", windir)])
    }
    #[cfg(not(windows))]
    BTreeMap::new()
}

pub trait CommandExt {
    /// Deliberately strip the environment to test Rune in isolation, restoring
    /// only Windows' OS requirements. On Unix this remains an empty environment.
    fn isolated_env(&mut self) -> &mut Self;

    /// Add executable lookup to an isolated child by copying only the real PATH.
    /// Call after `isolated_env`; explicit fixture settings remain intact, and
    /// no parent Rune settings, credentials, proxies, or config directories leak.
    fn with_external_programs(&mut self) -> &mut Self;
}

impl CommandExt for Command {
    fn isolated_env(&mut self) -> &mut Self {
        self.env_clear().envs(os_environment())
    }

    fn with_external_programs(&mut self) -> &mut Self {
        self.env("PATH", std::env::var_os("PATH").unwrap_or_default())
    }
}

/// Give embedded Python fixtures the same OS allowlist without a second copy.
/// The Python helper leaves each fixture's Linux environment exactly intact.
pub fn python_script(script: &str) -> String {
    let environment: BTreeMap<_, _> = os_environment()
        .into_iter()
        .map(|(key, value)| {
            (
                key,
                value.into_string().expect("Unicode Windows OS directory"),
            )
        })
        .collect();
    let environment = serde_json::to_string(&environment).expect("OS environment JSON");
    format!(
        "def child_environment(environment):\n    environment.update({environment})\n    return environment\n\n{script}"
    )
}
