//! Scratch drafts for the user's trusted editor command, and the transcript
//! handed to a pager.

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
    let scratch = Scratch::new("rune-draft")?;
    let path = scratch.write("draft.txt", draft)?;
    let status = command_for(editor, &path).status()?;
    if !status.success() {
        return Err(io::Error::other(format!("editor exited with {status}")));
    }
    std::fs::read_to_string(path)
}

/// Shows `text` in the user's pager and waits for it to exit.
pub(crate) fn page(text: &str) -> io::Result<()> {
    let pager = configured_pager(
        std::env::var_os("PAGER"),
        on_path("less"),
        std::env::var_os("VISUAL"),
        std::env::var_os("EDITOR"),
    );
    page_with(text, &pager)
}

/// `PAGER`, else `less` when it is installed, else `VISUAL`, else `EDITOR`,
/// else `more`.
fn configured_pager(
    pager: Option<OsString>,
    less: bool,
    visual: Option<OsString>,
    editor: Option<OsString>,
) -> OsString {
    let set = |value: Option<OsString>| value.filter(|value| !value.is_empty());
    set(pager)
        .or_else(|| (less && !cfg!(windows)).then(|| OsString::from("less")))
        .or_else(|| set(visual))
        .or_else(|| set(editor))
        .unwrap_or_else(|| OsString::from("more"))
}

/// Whether a program of that name is in a `PATH` directory.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
    })
}

fn page_with(text: &str, pager: &std::ffi::OsStr) -> io::Result<()> {
    let scratch = Scratch::new("rune-transcript")?;
    let path = scratch.write("transcript.txt", text)?;
    // A pager that exits with a failure has still been shown; only a failure
    // to start is reported, which the shell reports as status 127.
    let status = command_for(pager, &path).status()?;
    if status.code() == Some(127) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("`{}` was not found", pager.to_string_lossy()),
        ));
    }
    Ok(())
}

/// Runs the configured command with the scratch path as its last argument.
fn command_for(program: &std::ffi::OsStr, path: &std::path::Path) -> Command {
    // The configured command may contain arguments and quoting. The scratch
    // path is passed separately so it never becomes shell syntax.
    #[cfg(not(windows))]
    let command = {
        let mut script = program.to_os_string();
        script.push(" \"$1\"");
        let mut command = Command::new("sh");
        command.arg("-c").arg(script).arg("rune-editor").arg(path);
        command
    };
    #[cfg(windows)]
    let command = {
        let mut script = program.to_os_string();
        script.push(" \"%RUNE_EDITOR_DRAFT%\"");
        let mut command = Command::new("cmd");
        command.args(["/D", "/S", "/C"]).arg(script);
        command.env("RUNE_EDITOR_DRAFT", path);
        command
    };
    command
}

/// A private directory also permits editors that save by replacing the file.
struct Scratch(PathBuf);

impl Scratch {
    /// Writes a file only this user can read.
    fn write(&self, name: &str, text: &str) -> io::Result<PathBuf> {
        let path = self.0.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(text.as_bytes())?;
        Ok(path)
    }

    fn new(prefix: &str) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "{prefix}-{}-{stamp}-{}",
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

    #[test]
    fn pager_selection_prefers_pager_then_less_then_editors_then_more() {
        let pick = |pager: Option<&str>, less, visual: Option<&str>, editor: Option<&str>| {
            configured_pager(
                pager.map(OsString::from),
                less,
                visual.map(OsString::from),
                editor.map(OsString::from),
            )
        };
        assert_eq!(pick(Some("most"), true, Some("v"), Some("e")), "most");
        if !cfg!(windows) {
            assert_eq!(pick(Some(""), true, Some("v"), None), "less");
        }
        assert_eq!(pick(None, false, Some("v"), Some("e")), "v");
        assert_eq!(pick(None, false, Some(""), Some("e")), "e");
        assert_eq!(pick(None, false, None, None), "more");
    }

    #[cfg(unix)]
    #[test]
    fn pager_reads_a_private_copy_that_is_removed_after() {
        let dir = tempfile::tempdir().expect("fixture");
        let record = dir.path().join("record");
        let pager = format!(
            "sh -c 'cat \"$1\" > {0}; ls -l \"$1\" | cut -c1-10 >> {0}; printf %s \"$1\" > {0}.path' pager",
            record.display()
        );
        page_with("line one\nline two\n", std::ffi::OsStr::new(&pager)).expect("paged");
        let seen = std::fs::read_to_string(&record).expect("record");
        assert_eq!(seen, "line one\nline two\n-rw-------\n");
        let path = PathBuf::from(
            std::fs::read_to_string(record.with_extension("path")).expect("path record"),
        );
        assert!(!path.exists());
        assert!(!path.parent().expect("scratch directory").exists());
        assert!(page_with("x", std::ffi::OsStr::new("exit 3")).is_ok());
        let missing = page_with("x", std::ffi::OsStr::new("rune-no-such-pager"))
            .expect_err("a pager that cannot start");
        assert_eq!(missing.to_string(), "`rune-no-such-pager` was not found");
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
