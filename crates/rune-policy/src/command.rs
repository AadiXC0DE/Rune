//! Command classification by argument vector.
//!
//! Classification reads argv and never re-parses a shell string. A command that
//! arrives as argv also runs as argv, so a shell function or alias defined in a
//! user rc file cannot change what runs. The only route that involves a shell is
//! [`shell_argv`], and the string it runs is byte for byte the string a reviewer
//! saw.
//!
//! A command carrying a shell operator is never read-only. The argv may have
//! come from a shell, and a pipe or a redirect means the visible program is not
//! everything the command line does.

use std::fmt;

use serde::{Deserialize, Serialize};

/// What a command can do to the machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    /// Changes nothing.
    ReadOnly,
    /// Changes state that can be put back.
    Reversible,
    /// Destroys data or changes the machine in a way that cannot be undone.
    Consequential,
}

impl CommandKind {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Reversible => "reversible",
            Self::Consequential => "consequential",
        }
    }

    /// Returns true when the command changes nothing.
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::ReadOnly)
    }
}

impl fmt::Display for CommandKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The classification of one command, with the rule that produced it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Classification {
    /// What the command can do.
    pub kind: CommandKind,
    /// The rule that decided it, shown when a user asks why.
    pub reason: &'static str,
}

impl Classification {
    fn new(kind: CommandKind, reason: &'static str) -> Self {
        Self { kind, reason }
    }

    /// Returns true when the command changes nothing.
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        self.kind.is_read_only()
    }
}

const REASON_EMPTY_ARGV: &str = "argv is empty";
const REASON_PIPE: &str = "argv carries a pipe";
const REASON_REDIRECT: &str = "argv carries a redirect";
const REASON_SUBSTITUTION: &str = "argv carries a command or variable substitution";
const REASON_SEPARATOR: &str = "argv carries a command separator";
const REASON_READ_ONLY_PROGRAM: &str = "the program only reads";
const REASON_READ_ONLY_GIT: &str = "the git subcommand only reads";
const REASON_WRITING_FLAG: &str = "a flag makes the program write";
const REASON_DESTRUCTIVE_PROGRAM: &str = "the program destroys data or the machine";
const REASON_DESTRUCTIVE_SUBCOMMAND: &str =
    "the subcommand destroys state or publishes an artifact";
const REASON_FILESYSTEM_ROOT: &str = "an argument names a filesystem root";
const REASON_SHELL_SCRIPT: &str = "the shell script names a destructive program";
const REASON_DEFAULT: &str = "no rule matched, so the command is treated as reversible";

/// Programs that only read, when they appear on the path or by bare name.
const READ_ONLY_PROGRAMS: &[&str] = &[
    "basename",
    "b3sum",
    "cat",
    "cksum",
    "cmp",
    "column",
    "comm",
    "cut",
    "date",
    "df",
    "diff",
    "dirname",
    "du",
    "echo",
    "env",
    "fd",
    "file",
    "find",
    "false",
    "getconf",
    "grep",
    "head",
    "hexdump",
    "hostname",
    "id",
    "jq",
    "locale",
    "ls",
    "md5sum",
    "nl",
    "nproc",
    "od",
    "printenv",
    "printf",
    "pwd",
    "readlink",
    "realpath",
    "rev",
    "rg",
    "seq",
    "sha1sum",
    "sha256sum",
    "shasum",
    "sort",
    "stat",
    "tac",
    "tail",
    "test",
    "tr",
    "tree",
    "true",
    "tty",
    "type",
    "uname",
    "uniq",
    "wc",
    "which",
    "whoami",
    "xxd",
];

/// Programs that destroy data or change the machine itself.
const DESTRUCTIVE_PROGRAMS: &[&str] = &[
    "blkdiscard",
    "chattr",
    "chgrp",
    "chmod",
    "chown",
    "crontab",
    "dd",
    "diskpart",
    "diskutil",
    "doas",
    "fdisk",
    "format",
    "groupadd",
    "groupdel",
    "halt",
    "iptables",
    "kill",
    "killall",
    "launchctl",
    "mkfs",
    "mkswap",
    "mount",
    "mv",
    "nft",
    "parted",
    "passwd",
    "pfctl",
    "pkill",
    "poweroff",
    "reboot",
    "rm",
    "rmdir",
    "setfacl",
    "shred",
    "shutdown",
    "su",
    "sudo",
    "swapoff",
    "swapon",
    "systemctl",
    "truncate",
    "umount",
    "unlink",
    "useradd",
    "userdel",
    "usermod",
    "wipefs",
];

/// Program names whose dotted forms are the same tool, such as `mkfs.ext4`.
const DESTRUCTIVE_FAMILIES: &[&str] = &["mkfs"];

/// Programs whose arguments begin with the name of a subcommand.
const PUBLISHERS: &[(&str, &[&str])] = &[
    ("bun", &["publish"]),
    ("cargo", &["publish"]),
    ("gem", &["push"]),
    ("npm", &["publish"]),
    ("pnpm", &["publish"]),
    ("poetry", &["publish"]),
    ("twine", &["upload"]),
    ("yarn", &["publish"]),
];

/// Git subcommands that rewrite or publish history.
const GIT_DESTRUCTIVE: &[&str] = &["clean", "filter-branch", "push"];

/// Git subcommands that only read.
const GIT_READ_ONLY: &[&str] = &[
    "blame",
    "branch",
    "cat-file",
    "check-ignore",
    "count-objects",
    "describe",
    "diff",
    "diff-tree",
    "log",
    "ls-files",
    "ls-remote",
    "merge-base",
    "name-rev",
    "rev-parse",
    "shortlog",
    "show",
    "status",
];

/// Flags that turn `git branch` from a listing into a mutation.
const GIT_BRANCH_WRITES: &[&str] = &[
    "-C",
    "-D",
    "-M",
    "-c",
    "-d",
    "-f",
    "-m",
    "-u",
    "--copy",
    "--delete",
    "--edit-description",
    "--force",
    "--move",
    "--set-upstream-to",
    "--unset-upstream",
];

/// Git global options that consume the argument after them.
const GIT_VALUE_OPTIONS: &[&str] = &[
    "-C",
    "-c",
    "--attr-source",
    "--config-env",
    "--exec-path",
    "--git-dir",
    "--namespace",
    "--super-prefix",
    "--work-tree",
];

/// Programs that only forward to the command that follows them.
const WRAPPERS: &[&str] = &["env", "nice", "nohup", "time"];

/// Wrapper flags that consume the argument after them.
const WRAPPER_VALUE_FLAGS: &[&str] = &[
    "-C",
    "-S",
    "-c",
    "-n",
    "-u",
    "--adjustment",
    "--chdir",
    "--class",
    "--split-string",
    "--unset",
];

/// Flags that make an otherwise read-only program write. A trailing `*` marks
/// a prefix, which is how `find -fprint` and `find -fprintf` are covered.
const WRITING_FLAGS: &[(&str, &[&str])] = &[
    ("date", &["--set", "-s"]),
    (
        "find",
        &[
            "-delete", "-exec", "-execdir", "-fls*", "-fprint*", "-ok", "-okdir",
        ],
    ),
    ("jq", &["--in-place", "-i"]),
    ("rg", &["--pre"]),
    ("sort", &["--compress-program", "--output", "-o"]),
    ("tree", &["--output", "-o"]),
];

/// Interpreters that run a script string given to `-c`.
const SHELLS: &[&str] = &["bash", "dash", "fish", "ksh", "sh", "zsh"];

/// Directories whose contents are the system's own programs.
const SYSTEM_BIN_DIRS: &[&str] = &[
    "/bin/",
    "/opt/homebrew/bin/",
    "/opt/homebrew/sbin/",
    "/sbin/",
    "/usr/bin/",
    "/usr/local/bin/",
    "/usr/local/sbin/",
    "/usr/sbin/",
];

/// Characters a shell gives meaning to that plain argument passing does not.
///
/// `#` starts a comment at the beginning of a word and a newline ends the
/// command, so both change what runs and neither survives whitespace splitting.
const SHELL_MEANINGFUL: &[char] = &[
    '\n', '!', '"', '#', '$', '%', '&', '\'', '(', ')', '*', ';', '<', '>', '?', '[', '\\', ']',
    '`', '{', '}', '|', '~',
];

/// The shell used when a command string needs one.
#[cfg(unix)]
const SHELL_PROGRAM: &str = "/bin/sh";
/// The flag that hands a shell one string to interpret.
#[cfg(unix)]
const SHELL_FLAG: &str = "-c";
/// The shell used when a command string needs one.
#[cfg(not(unix))]
const SHELL_PROGRAM: &str = "cmd.exe";
/// The flag that hands a shell one string to interpret.
#[cfg(not(unix))]
const SHELL_FLAG: &str = "/C";

/// Classifies a command from its argument vector.
///
/// The result is derived from argv alone. Nothing here consults a shell, a rc
/// file, or an alias, so the classification and the program that runs are the
/// same decision.
#[must_use]
pub fn classify(argv: &[String]) -> Classification {
    if argv.is_empty() {
        return Classification::new(CommandKind::Reversible, REASON_EMPTY_ARGV);
    }
    if let Some(classification) = consequential(argv) {
        return classification;
    }
    match shell_operator(argv) {
        // The visible program is not the whole command line, so the read-only
        // allowlist does not apply to what the shell would do with it.
        Some(reason) => Classification::new(CommandKind::Reversible, reason),
        None => read_only_verdict(argv)
            .unwrap_or_else(|reason| Classification::new(CommandKind::Reversible, reason)),
    }
}

/// Returns true when a command string has to be interpreted by a shell.
///
/// Quoting, escaping, globbing, and expansion all need one, because the string
/// cannot be split into the argv the program would receive. A plain command such
/// as `ls -la` does not.
#[must_use]
pub fn requires_shell(command: &str) -> bool {
    command.trim().is_empty() || command.contains(SHELL_MEANINGFUL)
}

/// Returns the exact argv that runs a command string through a shell.
///
/// The string is one argument, so what runs is what was reviewed. The shell is
/// absolute and non-interactive: `/bin/sh -c` reads no rc file, so neither an
/// alias nor a function can substitute a different program for the first word.
#[must_use]
pub fn shell_argv(command: &str) -> Vec<String> {
    vec![
        SHELL_PROGRAM.to_owned(),
        SHELL_FLAG.to_owned(),
        command.to_owned(),
    ]
}

/// Returns the argv for a command string that needs no shell.
///
/// `None` means the string needs a shell, so it has to go through
/// [`shell_argv`]. Splitting on whitespace is exact here: a string holding
/// quoting, escaping, a glob, or an expansion is rejected by [`requires_shell`].
#[must_use]
pub fn direct_argv(command: &str) -> Option<Vec<String>> {
    if requires_shell(command) {
        return None;
    }
    Some(command.split_whitespace().map(str::to_owned).collect())
}

/// Returns the operator that makes a command line more than one program.
fn shell_operator(argv: &[String]) -> Option<&'static str> {
    for token in argv {
        if token.contains('|') {
            return Some(REASON_PIPE);
        }
        if token.contains(['<', '>']) {
            return Some(REASON_REDIRECT);
        }
        if token.contains(['$', '`']) {
            return Some(REASON_SUBSTITUTION);
        }
        if token.contains(['&', ';', '\n']) {
            return Some(REASON_SEPARATOR);
        }
    }
    None
}

/// Returns the destructive classification, when one applies.
fn consequential(argv: &[String]) -> Option<Classification> {
    // A root argument is destructive wherever it appears: `rm -rf /` and
    // `find / -delete` both start there.
    if argv.iter().any(|argument| is_filesystem_root(argument)) {
        return Some(Classification::new(
            CommandKind::Consequential,
            REASON_FILESYSTEM_ROOT,
        ));
    }

    let effective = effective_argv(argv);
    let program = program_name(effective.first()?);
    let arguments = effective.get(1..).unwrap_or_default();

    if DESTRUCTIVE_PROGRAMS.contains(&program)
        || DESTRUCTIVE_FAMILIES
            .iter()
            .any(|family| in_family(program, family))
    {
        return Some(Classification::new(
            CommandKind::Consequential,
            REASON_DESTRUCTIVE_PROGRAM,
        ));
    }

    if let Some((_, subcommands)) = PUBLISHERS.iter().find(|(name, _)| *name == program)
        && arguments
            .iter()
            .filter(|argument| !argument.starts_with('-'))
            .take(3)
            .any(|argument| subcommands.contains(&argument.as_str()))
    {
        return Some(Classification::new(
            CommandKind::Consequential,
            REASON_DESTRUCTIVE_SUBCOMMAND,
        ));
    }

    if program == "git" {
        return git_destructive(arguments);
    }

    if SHELLS.contains(&program) {
        return shell_script(arguments);
    }

    None
}

/// Returns the read-only classification, or why the command is not read-only.
fn read_only_verdict(argv: &[String]) -> Result<Classification, &'static str> {
    let effective = effective_argv(argv);
    let program = program_name(effective.first().ok_or(REASON_DEFAULT)?);
    if program == "git" {
        return git_read_only(effective.get(1..).unwrap_or_default());
    }
    if !READ_ONLY_PROGRAMS.contains(&program) {
        return Err(REASON_DEFAULT);
    }
    let arguments = effective.get(1..).unwrap_or_default();
    if arguments
        .iter()
        .any(|argument| is_writing_flag(program, argument))
    {
        return Err(REASON_WRITING_FLAG);
    }
    Ok(Classification::new(
        CommandKind::ReadOnly,
        REASON_READ_ONLY_PROGRAM,
    ))
}

/// Returns the destructive git cases.
fn git_destructive(args: &[String]) -> Option<Classification> {
    let (subcommand, arguments) = git_subcommand(args)?;
    let hard_reset = subcommand == "reset" && arguments.iter().any(|argument| argument == "--hard");
    if GIT_DESTRUCTIVE.contains(&subcommand) || hard_reset {
        return Some(Classification::new(
            CommandKind::Consequential,
            REASON_DESTRUCTIVE_SUBCOMMAND,
        ));
    }
    None
}

/// Returns the read-only git cases.
fn git_read_only(args: &[String]) -> Result<Classification, &'static str> {
    let Some((subcommand, arguments)) = git_subcommand(args) else {
        return Err(REASON_DEFAULT);
    };
    if !GIT_READ_ONLY.contains(&subcommand) {
        return Err(REASON_DEFAULT);
    }
    if subcommand == "branch"
        && arguments
            .iter()
            .any(|argument| GIT_BRANCH_WRITES.contains(&argument.as_str()))
    {
        return Err(REASON_WRITING_FLAG);
    }
    Ok(Classification::new(
        CommandKind::ReadOnly,
        REASON_READ_ONLY_GIT,
    ))
}

/// Returns the git subcommand and the arguments after it.
fn git_subcommand(args: &[String]) -> Option<(&str, &[String])> {
    let mut index = 0_usize;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            index = index.saturating_add(1);
            break;
        }
        if argument.starts_with('-') {
            let glued = argument.starts_with("--") && argument.contains('=');
            let step = if glued || !GIT_VALUE_OPTIONS.contains(&argument.as_str()) {
                1
            } else {
                2
            };
            index = index.saturating_add(step);
            continue;
        }
        break;
    }
    let subcommand = args.get(index)?.as_str();
    Some((
        subcommand,
        args.get(index.saturating_add(1)..).unwrap_or_default(),
    ))
}

/// Returns the destructive classification for a shell script argument.
///
/// Reading the script to escalate is safe in one direction only: it can make a
/// command more restricted, never more trusted. The read-only path never looks
/// inside it.
fn shell_script(args: &[String]) -> Option<Classification> {
    let script = script_argument(args)?;
    let mut tokens = script.split_whitespace();
    let first = tokens.next()?;
    let program = program_name(first);
    let destructive = DESTRUCTIVE_PROGRAMS.contains(&program)
        || DESTRUCTIVE_FAMILIES
            .iter()
            .any(|family| in_family(program, family))
        || script.split_whitespace().any(is_filesystem_root);
    if destructive {
        return Some(Classification::new(
            CommandKind::Consequential,
            REASON_SHELL_SCRIPT,
        ));
    }
    None
}

/// Returns the string a shell was asked to interpret.
///
/// Only `-c` and its combined short forms take a string. Anything else, such as
/// a script path, is not a command line this module can read.
fn script_argument(args: &[String]) -> Option<&str> {
    let mut index = 0_usize;
    while let Some(argument) = args.get(index) {
        let inline =
            argument.starts_with('-') && !argument.starts_with("--") && argument.contains('c');
        if inline || argument == "-c" || argument == "--command" {
            return args.get(index.saturating_add(1)).map(String::as_str);
        }
        if !argument.starts_with('-') {
            return None;
        }
        index = index.saturating_add(1);
    }
    None
}

/// Strips the wrapper programs that only forward to the real one.
fn effective_argv(argv: &[String]) -> &[String] {
    let mut args = argv;
    while let Some(program) = args.first() {
        if !WRAPPERS.contains(&program_name(program)) {
            return args;
        }
        let Some(rest) = skip_wrapper(program_name(program), args.get(1..).unwrap_or_default())
        else {
            return args;
        };
        // A wrapper with nothing left to run is the program: `env` alone prints
        // the environment while `env ls` lists.
        if rest.is_empty() || rest.len() == args.len() {
            return args;
        }
        args = rest;
    }
    args
}

/// Returns the arguments after a wrapper's own options.
fn skip_wrapper<'a>(program: &str, args: &'a [String]) -> Option<&'a [String]> {
    let mut index = 0_usize;
    while let Some(argument) = args.get(index) {
        if argument.starts_with('-') {
            let step = if WRAPPER_VALUE_FLAGS.contains(&argument.as_str()) {
                2
            } else {
                1
            };
            index = index.saturating_add(step);
            continue;
        }
        if program == "env" && is_assignment(argument) {
            index = index.saturating_add(1);
            continue;
        }
        break;
    }
    args.get(index..)
}

/// Returns true for an `env` style `NAME=value` argument.
fn is_assignment(argument: &str) -> bool {
    match argument.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }
        None => false,
    }
}

/// Returns true when a flag makes one program write.
fn is_writing_flag(program: &str, argument: &str) -> bool {
    let Some((_, flags)) = WRITING_FLAGS.iter().find(|(name, _)| *name == program) else {
        return false;
    };
    flags.iter().any(|flag| {
        if let Some(prefix) = flag.strip_suffix('*') {
            return argument.starts_with(prefix);
        }
        if argument == *flag {
            return true;
        }
        match argument.strip_prefix(flag) {
            // `--output=file` and its short form `-ofile` both carry a value
            // without a separating space, so an attached value still writes.
            Some(rest) => rest.starts_with('=') || is_short_attached(flag, rest),
            None => false,
        }
    })
}

/// Returns true when a short flag carries its value with no separating space.
fn is_short_attached(flag: &str, rest: &str) -> bool {
    let mut characters = flag.chars();
    matches!(characters.next(), Some('-'))
        && characters
            .next()
            .is_some_and(|_| characters.next().is_none())
        && !rest.is_empty()
}

/// Returns true when a program name belongs to a dotted family.
fn in_family(program: &str, family: &str) -> bool {
    program == family
        || program
            .strip_prefix(family)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Returns true when an argument names the root of a filesystem.
///
/// `rm -rf /` erases the machine, and the same argument reaches any program, so
/// the check does not care which one it is.
fn is_filesystem_root(argument: &str) -> bool {
    if argument.is_empty() {
        return false;
    }
    let trimmed = argument.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return true;
    }
    let bytes = trimmed.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Returns the program name an argv element names.
///
/// A path is reduced to its base name only inside a system binary directory.
/// `./ls` and `/tmp/ls` stay as written, so a file in the workspace cannot
/// borrow the name of an allowlisted program.
fn program_name(argument: &str) -> &str {
    for prefix in SYSTEM_BIN_DIRS {
        if let Some(base) = argument.strip_prefix(prefix) {
            return base;
        }
    }
    argument
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    fn kind(parts: &[&str]) -> CommandKind {
        classify(&argv(parts)).kind
    }

    #[test]
    fn an_allowlisted_command_with_no_operator_is_read_only() {
        for parts in [
            vec!["ls", "-la"],
            vec!["cat", "Cargo.toml"],
            vec!["grep", "-rn", "pattern", "src"],
            vec!["git", "status"],
            vec!["git", "log", "--oneline"],
            vec!["git", "diff", "HEAD"],
            vec!["git", "show", "HEAD"],
            vec!["git", "branch"],
            vec!["wc", "-l", "README.md"],
            vec!["jq", ".name", "package.json"],
            vec!["/bin/ls", "-l"],
            vec!["/usr/bin/git", "status"],
        ] {
            let classification = classify(&argv(&parts));
            assert_eq!(
                classification.kind,
                CommandKind::ReadOnly,
                "{parts:?}: {}",
                classification.reason
            );
        }
    }

    #[test]
    fn an_allowlisted_command_with_a_pipe_or_redirect_is_not_read_only() {
        for (parts, reason) in [
            (vec!["ls", "|", "wc"], REASON_PIPE),
            (vec!["grep", "x", "f.txt", "|", "sort"], REASON_PIPE),
            (vec!["ls", ">", "out.txt"], REASON_REDIRECT),
            (vec!["ls>out.txt"], REASON_REDIRECT),
            (vec!["cat", "file", ">>", "log"], REASON_REDIRECT),
            (vec!["cat", "<", "file"], REASON_REDIRECT),
            (vec!["echo", "$HOME"], REASON_SUBSTITUTION),
            (vec!["echo", "`whoami`"], REASON_SUBSTITUTION),
            (vec!["ls", "&&", "ls"], REASON_SEPARATOR),
            (vec!["ls", ";", "ls"], REASON_SEPARATOR),
            (vec!["ls", "&"], REASON_SEPARATOR),
        ] {
            let classification = classify(&argv(&parts));
            assert_eq!(classification.kind, CommandKind::Reversible, "{parts:?}");
            assert_eq!(classification.reason, reason, "{parts:?}");
        }
    }

    #[test]
    fn a_shell_route_to_an_allowlisted_program_is_never_read_only() {
        assert_eq!(kind(&["sh", "-c", "ls"]), CommandKind::Reversible);
        assert_eq!(kind(&["bash", "-c", "cat x"]), CommandKind::Reversible);
        // A string route is classified from the argv that runs it, so the
        // operator inside the script keeps it out of the read-only class.
        for script in ["ls > out.txt", "ls | wc -l", "git status"] {
            assert_eq!(
                classify(&shell_argv(script)).kind,
                CommandKind::Reversible,
                "{script}"
            );
        }
    }

    #[test]
    fn destructive_programs_are_consequential() {
        for parts in [
            vec!["rm", "-rf", "src"],
            vec!["rm", "-rf", "/"],
            vec!["mv", "a", "b"],
            vec!["dd", "if=/dev/zero", "of=/dev/disk0"],
            vec!["mkfs.ext4", "/dev/disk1"],
            vec!["shutdown", "-h", "now"],
            vec!["reboot"],
            vec!["kill", "-9", "1234"],
            vec!["chmod", "777", "script.sh"],
            vec!["chown", "root", "file"],
            vec!["sudo", "ls"],
            vec!["systemctl", "stop", "nginx"],
        ] {
            let classification = classify(&argv(&parts));
            assert_eq!(
                classification.kind,
                CommandKind::Consequential,
                "{parts:?}: {}",
                classification.reason
            );
        }
    }

    #[test]
    fn destructive_git_and_publish_subcommands_are_consequential() {
        for parts in [
            vec!["git", "push", "origin", "main"],
            vec!["git", "push"],
            vec!["git", "reset", "--hard", "HEAD~1"],
            vec!["git", "clean", "-fd"],
            vec!["git", "-C", "/repo", "push"],
            vec!["npm", "publish"],
            vec!["npm", "publish", "--access", "public"],
            vec!["npm", "--silent", "publish"],
            vec!["yarn", "npm", "publish"],
            vec!["pnpm", "publish"],
            vec!["bun", "publish"],
            vec!["cargo", "publish"],
            vec!["gem", "push", "gem.gem"],
        ] {
            let classification = classify(&argv(&parts));
            assert_eq!(
                classification.kind,
                CommandKind::Consequential,
                "{parts:?}: {}",
                classification.reason
            );
        }
    }

    #[test]
    fn a_bare_root_argument_is_consequential_whatever_the_program_is() {
        for parts in [vec!["echo", "/"], vec!["ls", "/"], vec!["du", "//"]] {
            let classification = classify(&argv(&parts));
            assert_eq!(classification.kind, CommandKind::Consequential, "{parts:?}");
            assert_eq!(classification.reason, REASON_FILESYSTEM_ROOT);
        }
        // A path that merely starts at the root is not the root.
        assert_eq!(kind(&["cat", "/etc/hosts"]), CommandKind::ReadOnly);
        assert_eq!(kind(&["ls", "./"]), CommandKind::ReadOnly);
    }

    #[test]
    fn a_shell_script_naming_a_destructive_program_is_consequential() {
        for parts in [
            vec!["sh", "-c", "rm -rf /"],
            vec!["bash", "-c", "sudo rm -rf build"],
            vec!["bash", "-lc", "dd if=/dev/zero of=/dev/disk0"],
            vec!["zsh", "-c", "echo /"],
        ] {
            let classification = classify(&argv(&parts));
            assert_eq!(
                classification.kind,
                CommandKind::Consequential,
                "{parts:?}: {}",
                classification.reason
            );
        }
        assert_eq!(kind(&["sh", "-c", "ls src"]), CommandKind::Reversible);
        // A script path is a file whose contents are not read here, and the
        // argument after `--` is not a command string either.
        assert_eq!(kind(&["bash", "deploy.sh"]), CommandKind::Reversible);
        assert_eq!(
            kind(&["bash", "--", "rm", "-rf", "src"]),
            CommandKind::Reversible
        );
    }

    #[test]
    fn wrappers_do_not_hide_the_program() {
        assert_eq!(
            kind(&["env", "rm", "-rf", "src"]),
            CommandKind::Consequential
        );
        assert_eq!(
            kind(&["nice", "-n", "5", "rm", "src"]),
            CommandKind::Consequential
        );
        assert_eq!(kind(&["nohup", "git", "push"]), CommandKind::Consequential);
        assert_eq!(kind(&["env", "FOO=1", "ls"]), CommandKind::ReadOnly);
        assert_eq!(kind(&["env", "-i", "ls", "-l"]), CommandKind::ReadOnly);
        assert_eq!(kind(&["/usr/bin/env", "true"]), CommandKind::ReadOnly);
    }

    #[test]
    fn a_writing_flag_removes_the_read_only_classification() {
        for parts in [
            vec!["find", ".", "-delete"],
            vec!["find", ".", "-exec", "rm", "{}", "+"],
            vec!["find", ".", "-fprint0", "out"],
            vec!["sort", "-o", "out", "in"],
            vec!["sort", "--output=out", "in"],
            vec!["sort", "-oout", "in"],
            vec!["date", "-s", "2026-01-01"],
            vec!["date", "--set=2026-01-01"],
            vec!["date", "-s2026-01-01"],
            vec!["git", "branch", "-D", "feature"],
            vec!["git", "branch", "--delete", "feature"],
        ] {
            let classification = classify(&argv(&parts));
            assert_ne!(classification.kind, CommandKind::ReadOnly, "{parts:?}");
            assert_eq!(classification.reason, REASON_WRITING_FLAG, "{parts:?}");
        }
        assert_eq!(kind(&["find", ".", "-name", "*.rs"]), CommandKind::ReadOnly);
        assert_eq!(kind(&["git", "branch", "--list"]), CommandKind::ReadOnly);
        // A file argument that merely starts with the same letters is not a flag.
        assert_eq!(kind(&["cat", "-output.txt"]), CommandKind::ReadOnly);
    }

    #[test]
    fn a_mutating_git_subcommand_is_reversible() {
        for parts in [
            vec!["git", "commit", "-m", "x"],
            vec!["git", "add", "."],
            vec!["git", "checkout", "main"],
            vec!["git", "reset", "HEAD~1"],
            vec!["git", "pull"],
            vec!["git", "fetch"],
            vec!["git", "unknown-subcommand"],
        ] {
            let classification = classify(&argv(&parts));
            assert_eq!(classification.kind, CommandKind::Reversible, "{parts:?}");
        }
    }

    #[test]
    fn a_path_outside_the_system_directories_keeps_its_name() {
        assert_eq!(kind(&["./ls"]), CommandKind::Reversible);
        assert_eq!(kind(&["/tmp/ls"]), CommandKind::Reversible);
        assert_eq!(kind(&["scripts/ls"]), CommandKind::Reversible);
        assert_eq!(kind(&["/bin/../ls"]), CommandKind::Reversible);
    }

    #[test]
    fn an_unknown_program_is_reversible() {
        for parts in [
            vec!["python3", "-c", "print(1)"],
            vec!["npm", "test"],
            vec!["cargo", "test"],
            vec!["make", "build"],
            vec!["xargs", "rm"],
            vec!["sed", "-n", "1p", "f"],
            vec!["tee", "out"],
        ] {
            let classification = classify(&argv(&parts));
            assert_eq!(classification.kind, CommandKind::Reversible, "{parts:?}");
            assert_eq!(classification.reason, REASON_DEFAULT, "{parts:?}");
        }
        let empty = classify(&[]);
        assert_eq!(empty.kind, CommandKind::Reversible);
        assert_eq!(empty.reason, REASON_EMPTY_ARGV);
    }

    #[test]
    fn a_command_string_without_shell_syntax_does_not_need_a_shell() {
        for command in [
            "ls -la",
            "git status",
            "cargo test --workspace",
            "cat a.txt",
        ] {
            assert!(!requires_shell(command), "{command}");
            let argv = direct_argv(command).expect("direct argv");
            assert_eq!(
                argv.first().map(String::as_str),
                command.split(' ').next(),
                "{command}"
            );
        }
        // An allowlisted command keeps its classification through the direct route.
        let argv = direct_argv("ls -la").expect("direct argv");
        assert_eq!(classify(&argv).kind, CommandKind::ReadOnly);
    }

    #[test]
    fn a_command_string_with_shell_syntax_needs_a_shell() {
        for command in [
            "ls | wc",
            "ls > out.txt",
            "echo $HOME",
            "echo $(whoami)",
            "echo `whoami`",
            "ls && ls",
            "ls *.rs",
            "ls 'a b'",
            "ls ~/src",
            "ls a\\ b",
            "ls # note",
            "ls\nrm -rf build",
            "",
            "   ",
        ] {
            assert!(requires_shell(command), "{command:?}");
            assert!(direct_argv(command).is_none(), "{command:?}");
        }
    }

    #[test]
    fn the_shell_argv_carries_the_string_unchanged() {
        let command = "printf '%s\\n' \"$HOME\" | head -1";
        let argv = shell_argv(command);
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[0], SHELL_PROGRAM);
        assert_eq!(argv[1], SHELL_FLAG);
        assert_eq!(argv[2], command);
    }

    #[test]
    fn a_variable_expansion_is_reviewed_and_run_as_the_same_string() {
        // What the reviewer reads is element two of the argv, which is the same
        // value passed to the exec call below and the same one the shell expands.
        let command = "printf '%s' \"$RUNE_APPROVAL_PROBE\"";
        assert!(requires_shell(command));
        let argv = shell_argv(command);
        assert_eq!(argv[2], command);

        #[cfg(unix)]
        {
            let output = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .env("RUNE_APPROVAL_PROBE", "expanded-once")
                .output()
                .expect("run");
            assert_eq!(String::from_utf8_lossy(&output.stdout), "expanded-once");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_shell_function_cannot_change_what_an_argv_command_runs() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let listing = dir.path().join("listing");
        std::fs::create_dir(&listing).expect("create dir");
        std::fs::write(listing.join("real-file.txt"), "").expect("write file");
        let rc = dir.path().join("rc");
        std::fs::write(
            &rc,
            "ls() { echo RUNE-HIJACKED; }\nalias ls='echo RUNE-HIJACKED'\n",
        )
        .expect("write rc");

        let string_form = format!("ls {}", listing.display());
        assert!(!requires_shell(&string_form));

        // The string form, run by a shell that has sourced the rc file, is
        // hijacked: this is what a re-parse would execute.
        let hijacked = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(". {}; {string_form}", rc.display()))
            .output()
            .expect("run hijacked");
        assert!(
            String::from_utf8_lossy(&hijacked.stdout).contains("RUNE-HIJACKED"),
            "the rc file did not hijack the shell route"
        );

        // The argv form is what classification saw and what the executor runs.
        let argv = direct_argv(&string_form).expect("direct argv");
        assert_eq!(argv, vec!["ls".to_owned(), listing.display().to_string()]);
        assert_eq!(classify(&argv).kind, CommandKind::ReadOnly);

        let direct = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .expect("run direct");
        let stdout = String::from_utf8_lossy(&direct.stdout);
        assert!(stdout.contains("real-file.txt"), "{stdout}");
        assert!(!stdout.contains("RUNE-HIJACKED"), "{stdout}");
    }

    #[test]
    fn command_kinds_render_their_wire_names() {
        assert_eq!(CommandKind::ReadOnly.as_str(), "read_only");
        assert_eq!(CommandKind::Reversible.as_str(), "reversible");
        assert_eq!(CommandKind::Consequential.as_str(), "consequential");
        assert_eq!(CommandKind::Consequential.to_string(), "consequential");
        assert!(CommandKind::ReadOnly.is_read_only());
        assert!(!CommandKind::Reversible.is_read_only());
    }
}
