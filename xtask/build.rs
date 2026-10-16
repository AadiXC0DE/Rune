//! Records the target the task binary is built for.
//!
//! The release task names an artifact after the platform that produced it, and
//! the compiler knows that name while the task runs.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/process_baseline.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Build a separate, dependency-free process. Timing xtask itself or a
        // command interpreter would subtract unrelated application work.
        let output =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?)
                .join("rune-process-baseline.exe");
        let status = std::process::Command::new(std::env::var_os("RUSTC").ok_or("missing RUSTC")?)
            .args([
                "--crate-name=rune_process_baseline",
                "--edition=2024",
                "--target",
                &target,
                "-Copt-level=z",
                "-Clto=fat",
                "-Ccodegen-units=1",
                "-Cpanic=abort",
                "-Cstrip=symbols",
                "src/process_baseline.rs",
                "-o",
            ])
            .arg(output)
            .status()?;
        if !status.success() {
            return Err("could not build the Windows process baseline".into());
        }
    }
    Ok(())
}
