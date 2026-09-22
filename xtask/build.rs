//! Records the target the task binary is built for.
//!
//! The release task names an artifact after the platform that produced it, and
//! the compiler knows that name while the task runs.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
