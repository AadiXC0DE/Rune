//! Live Linux isolation gate, including tests of the gate's failure paths.
#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::io::{ErrorKind, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::time::Duration;

use camino::Utf8PathBuf;
use rune_exec::{LinuxSandbox, PreparedCommand, Sandbox, SandboxPolicy, resource_limits, run};

const REQUIRED: &str = "RUNE_REQUIRE_LINUX_SANDBOX";
const LIVE_TEST: &str = "live_linux_network_isolation";

// Re-enter this test executable as the disposable command, so the socket
// fixture needs neither Python nor another executable or dependency.
#[test]
#[ignore = "child fixture invoked by the live isolation check"]
fn socket_connect_fixture() {
    let address: SocketAddr = std::env::var("RUNE_SOCKET_ADDRESS")
        .expect("fixture address")
        .parse()
        .expect("socket address");
    match TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
        Ok(_) => println!("SOCKET_CONNECTED"),
        Err(error) => {
            assert_ne!(error.kind(), ErrorKind::TimedOut, "{error}");
            println!("SOCKET_REFUSED: {error}");
        }
    }
    let contents = std::env::var("RUNE_SOURCE_CONTENTS").expect("source contents");
    std::fs::write("source.rs", contents).expect("sandbox must still allow source editing");
    println!("SOURCE_EDITED");
}

#[test]
fn live_linux_network_isolation() {
    let sandbox = LinuxSandbox::detect();
    let support = sandbox.support();
    if !support.is_full() {
        let reason = support.reason().expect("unavailable reason");
        // Bypass libtest capture so a normal workspace run also reports that
        // it verified no live isolation on this host.
        writeln!(
            std::io::stderr(),
            "Linux network isolation unavailable: {reason}"
        )
        .expect("unavailable isolation report");
        assert_ne!(
            std::env::var(REQUIRED).as_deref(),
            Ok("1"),
            "runner declares Linux sandbox support via {REQUIRED}=1: {reason}"
        );
        writeln!(
            std::io::stderr(),
            "SKIPPED live Linux network isolation; no enforcement verified"
        )
        .expect("skipped isolation report");
        return;
    }

    let guard = tempfile::tempdir().expect("disposable workspace");
    let workspace = Utf8PathBuf::from_path_buf(guard.path().to_path_buf()).expect("UTF-8 path");
    std::fs::write(workspace.join("source.rs"), "original").expect("source fixture");
    let listener = TcpListener::bind("127.0.0.1:0").expect("live socket fixture");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("listener address");
    // A target directory under /tmp is hidden by the sandbox's private tmpfs.
    // Put the disposable executable in the writable workspace instead.
    let executable = workspace.join("socket-fixture");
    std::fs::copy(
        std::env::current_exe().expect("test executable"),
        &executable,
    )
    .expect("disposable executable");
    let prepared = PreparedCommand {
        argv: vec![
            executable.as_str().to_owned(),
            "--exact".to_owned(),
            "socket_connect_fixture".to_owned(),
            "--ignored".to_owned(),
            "--nocapture".to_owned(),
        ],
        cwd: workspace.clone(),
        environment: BTreeMap::from([
            ("RUNE_SOCKET_ADDRESS".to_owned(), address.to_string()),
            ("RUNE_SOURCE_CONTENTS".to_owned(), "control".to_owned()),
        ]),
        limits: resource_limits(),
        reviewed: "disposable socket-connect and source-edit fixture".to_owned(),
    };
    // Prove the exact fixture and listener work before applying restrictions.
    let control = run(&prepared, Duration::from_secs(10), &|| false).expect("control run");
    assert!(
        control.exit.is_success() && !control.timed_out,
        "{control:?}"
    );
    assert!(control.stdout.contains("SOCKET_CONNECTED"), "{control:?}");
    listener.accept().expect("control reached live listener");

    let mut denied = prepared.clone();
    denied.environment.insert(
        "RUNE_SOURCE_CONTENTS".to_owned(),
        "isolated edit".to_owned(),
    );
    let policy = SandboxPolicy::new(workspace.clone(), Vec::new(), false);
    let wrapped = sandbox
        .wrap(&denied, &policy, false)
        .expect("enforced sandbox");
    let outcome = run(&wrapped, Duration::from_secs(10), &|| false).expect("sandbox run");
    assert!(
        outcome.exit.is_success() && !outcome.timed_out,
        "{outcome:?}"
    );
    assert!(
        outcome.stdout.contains("SOCKET_REFUSED:"),
        "sandbox fixture must report SOCKET_REFUSED: {outcome:?}"
    );
    assert!(!outcome.stdout.contains("SOCKET_CONNECTED"), "{outcome:?}");
    assert!(outcome.stdout.contains("SOURCE_EDITED"), "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(workspace.join("source.rs")).expect("edited source"),
        "isolated edit"
    );
    assert_eq!(
        listener.accept().expect_err("no sandbox connection").kind(),
        ErrorKind::WouldBlock
    );

    // The positive sandbox control also catches denial caused by a dead
    // listener, a broken fixture, or a backend that blocks every command.
    let granted = sandbox
        .wrap(
            &prepared,
            &SandboxPolicy::new(workspace.clone(), Vec::new(), true),
            false,
        )
        .expect("network grant");
    let control = run(&granted, Duration::from_secs(10), &|| false).expect("granted run");
    assert!(
        control.exit.is_success() && !control.timed_out,
        "{control:?}"
    );
    assert!(control.stdout.contains("SOCKET_CONNECTED"), "{control:?}");
    listener.accept().expect("granted sandbox reached listener");
    assert_eq!(
        std::fs::read_to_string(workspace.join("source.rs")).expect("source"),
        "control"
    );
    println!("VERIFIED live Linux network isolation: socket refused and source edited");
}

fn gate(path: &std::path::Path, required: bool) -> Output {
    Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", LIVE_TEST, "--nocapture"])
        .env("PATH", path)
        .env(REQUIRED, if required { "1" } else { "0" })
        .output()
        .expect("isolation gate")
}

#[test]
fn unavailable_isolation_is_explicit_and_fails_on_a_declared_runner() {
    for installed in [false, true] {
        let bin = tempfile::tempdir().expect("fixture PATH");
        if installed {
            let helper = bin.path().join("bwrap");
            std::fs::write(
                &helper,
                "#!/bin/sh\necho fixture-namespace-denied >&2\nexit 1\n",
            )
            .expect("unusable helper");
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))
                .expect("executable");
        }
        for required in [false, true] {
            let output = gate(bin.path(), required);
            assert_eq!(output.status.success(), !required, "{output:?}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("Linux network isolation unavailable:"),
                "{output:?}"
            );
            let reason = if installed {
                "fixture-namespace-denied"
            } else {
                "not on PATH"
            };
            assert!(stderr.contains(reason), "{output:?}");
            if required {
                assert!(
                    stderr.contains("runner declares Linux sandbox support"),
                    "{output:?}"
                );
            } else {
                assert!(stderr.contains("no enforcement verified"), "{output:?}");
            }
        }
    }
}

#[test]
fn the_gate_rejects_network_leaks_and_commands_that_never_run() {
    let bin = tempfile::tempdir().expect("helper directory");
    let helper = bin.path().join("bwrap");
    for command in ["exec \"$@\"", "exit 0"] {
        // A dishonest helper passes detection, then either runs without any
        // isolation or reports success without executing the socket fixture.
        std::fs::write(&helper, format!(
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = -- ]; then\n    shift\n    {command}\n  fi\n  shift\ndone\nexit 0\n"
        )).expect("fake helper");
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))
            .expect("executable");
        let output = gate(bin.path(), true);
        assert!(
            !output.status.success(),
            "false isolation passed: {output:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("isolation unavailable"), "{output:?}");
        assert!(stderr.contains("SOCKET_REFUSED:"), "{output:?}");
    }
}
