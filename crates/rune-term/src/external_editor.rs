//! Scratch drafts for the user's trusted editor command.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) fn edit(draft: &str) -> io::Result<String> {
    let editor = configured_editor(std::env::var_os("VISUAL"), std::env::var_os("EDITOR"));
    edit_with(draft, &editor)
}

fn configured_editor(visual: Option<OsString>, editor: Option<OsString>) -> OsString {
    visual
        .filter(|value| !value.is_empty())
        .or_else(|| editor.filter(|value| !value.is_empty()))
        .unwrap_or_else(|| OsString::from("vi"))
}

fn edit_with(draft: &str, editor: &std::ffi::OsStr) -> io::Result<String> {
    let scratch = Scratch::new()?;
    let path = scratch.0.join("draft.txt");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(draft.as_bytes())?;
    drop(file);

    // The configured command may contain arguments and quoting. The scratch
    // path is passed separately so it never becomes shell syntax.
    #[cfg(not(windows))]
    let mut command = {
        let mut script = editor.to_os_string();
        script.push(" \"$1\"");
        let mut command = Command::new("sh");
        command.arg("-c").arg(script).arg("rune-editor").arg(&path);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut script = editor.to_os_string();
        script.push(" \"%RUNE_EDITOR_DRAFT%\"");
        let mut command = Command::new("cmd");
        command.args(["/D", "/S", "/C"]).arg(script);
        command.env("RUNE_EDITOR_DRAFT", &path);
        command
    };
    let status = command.status()?;
    if !status.success() {
        return Err(io::Error::other(format!("editor exited with {status}")));
    }
    std::fs::read_to_string(path)
}

/// A private directory also permits editors that save by replacing the file.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "rune-draft-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create an editor scratch directory",
        ))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_selection_prefers_visual_then_editor_then_vi() {
        assert_eq!(
            configured_editor(Some("visual --wait".into()), Some("editor".into())),
            "visual --wait"
        );
        assert_eq!(
            configured_editor(Some("".into()), Some("editor".into())),
            "editor"
        );
        assert_eq!(configured_editor(None, Some("".into())), "vi");
    }

    #[cfg(unix)]
    #[test]
    fn editor_receives_exact_draft_and_scratch_is_removed_on_all_exits() {
        let dir = tempfile::tempdir().expect("fixture");
        let script = dir.path().join("editor with spaces.sh");
        let record = dir.path().join("path");
        std::fs::write(&script, format!(
            "test \"$(cat \"$2\")\" = 'draft 界' || exit 2\nprintf '%s' \"$2\" > '{}'\nprintf 'edited é\\nsecond\\n' > \"$2\"\nexit \"$1\"\n", record.display()
        )).expect("stub");
        for code in [0, 9] {
            let result = edit_with(
                "draft 界",
                std::ffi::OsStr::new(&format!("sh '{}' {code}", script.display())),
            );
            if code == 0 {
                assert_eq!(result.expect("edited"), "edited é\nsecond\n");
            } else {
                assert!(result.is_err());
            }
            let path = PathBuf::from(std::fs::read_to_string(&record).expect("record"));
            assert!(!path.exists());
            assert!(!path.parent().expect("scratch directory").exists());
        }
    }
}
