//! Pass and fail counts read from a test runner's output.
//!
//! A test run prints its verdict in one line that is easy to miss in a long
//! log. Reading the counts out lets a result lead with them, so the outcome is
//! the first thing seen rather than something found by scrolling. Only the
//! summary formats of common runners are recognised; output that matches none
//! of them yields nothing, and the log is shown as it is.

use std::fmt;

/// Totals from one test run.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    /// Tests that passed.
    pub passed: u64,
    /// Tests that failed.
    pub failed: u64,
    /// Tests that were skipped, ignored or pending.
    pub skipped: u64,
}

impl fmt::Display for Counts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} passed, {} failed", self.passed, self.failed)?;
        if self.skipped > 0 {
            write!(f, ", {} skipped", self.skipped)?;
        }
        Ok(())
    }
}

impl Counts {
    fn add(&mut self, other: Self) {
        self.passed = self.passed.saturating_add(other.passed);
        self.failed = self.failed.saturating_add(other.failed);
        self.skipped = self.skipped.saturating_add(other.skipped);
    }
}

/// Reads the counts of a test run from its output, when it has a summary.
#[must_use]
pub fn parse(output: &str) -> Option<Counts> {
    cargo(output)
        .or_else(|| tap(output))
        .or_else(|| jest(output))
        .or_else(|| vitest(output))
        .or_else(|| pytest(output))
        .or_else(|| mocha(output))
        .or_else(|| go(output))
}

/// Returns the number written immediately before `label` in `text`.
fn number_before(text: &str, label: &str) -> Option<u64> {
    let at = text.find(label)?;
    text[..at]
        .trim_end()
        .rsplit(|ch: char| !ch.is_ascii_digit())
        .next()
        .filter(|digits| !digits.is_empty())?
        .parse()
        .ok()
}

/// `test result: ok. 12 passed; 0 failed; 1 ignored; ...`, summed over binaries.
fn cargo(output: &str) -> Option<Counts> {
    let mut total: Option<Counts> = None;
    for line in output.lines() {
        let Some(rest) = line.trim().strip_prefix("test result: ") else {
            continue;
        };
        total.get_or_insert_with(Counts::default).add(Counts {
            passed: number_before(rest, " passed").unwrap_or_default(),
            failed: number_before(rest, " failed").unwrap_or_default(),
            skipped: number_before(rest, " ignored").unwrap_or_default(),
        });
    }
    total
}

/// `node --test` and other TAP producers: `# pass 3`, `# fail 1`, `# skipped 0`.
fn tap(output: &str) -> Option<Counts> {
    let mut counts = Counts::default();
    let mut seen = false;
    for line in output.lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("# ")
            .or_else(|| line.strip_prefix("\u{2139} "))
        else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let (Some(label), Some(value)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match label {
            "pass" => counts.passed = value,
            "fail" => counts.failed = value,
            "skipped" | "skip" | "todo" => counts.skipped = counts.skipped.saturating_add(value),
            _ => continue,
        }
        seen = true;
    }
    seen.then_some(counts)
}

/// `Tests:       1 failed, 3 passed, 4 total`.
fn jest(output: &str) -> Option<Counts> {
    let line = output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("Tests:"))?;
    Some(Counts {
        passed: number_before(line, " passed").unwrap_or_default(),
        failed: number_before(line, " failed").unwrap_or_default(),
        skipped: number_before(line, " skipped")
            .unwrap_or_default()
            .saturating_add(number_before(line, " todo").unwrap_or_default()),
    })
}

/// `Tests  1 failed | 3 passed (4)`.
fn vitest(output: &str) -> Option<Counts> {
    let line = output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("Tests ") && line.contains(" passed"))?;
    Some(Counts {
        passed: number_before(line, " passed").unwrap_or_default(),
        failed: number_before(line, " failed").unwrap_or_default(),
        skipped: number_before(line, " skipped").unwrap_or_default(),
    })
}

/// `=== 1 failed, 3 passed, 2 skipped in 0.12s ===`.
fn pytest(output: &str) -> Option<Counts> {
    let line = output.lines().rev().map(str::trim).find(|line| {
        line.starts_with('=')
            && line.ends_with('=')
            && (line.contains(" passed") || line.contains(" failed"))
            && line.contains(" in ")
    })?;
    Some(Counts {
        passed: number_before(line, " passed").unwrap_or_default(),
        failed: number_before(line, " failed")
            .unwrap_or_default()
            .saturating_add(number_before(line, " error").unwrap_or_default()),
        skipped: number_before(line, " skipped").unwrap_or_default(),
    })
}

/// `3 passing (12ms)`, `1 failing`, `2 pending`.
fn mocha(output: &str) -> Option<Counts> {
    let passing = output
        .lines()
        .map(str::trim)
        .find_map(|line| {
            line.ends_with(')')
                .then(|| number_before(line, " passing"))
                .flatten()
        })
        .or_else(|| {
            output.lines().map(str::trim).find_map(|line| {
                number_before(line, " passing").filter(|_| line.ends_with("passing"))
            })
        })?;
    let line_count = |label: &str| {
        output
            .lines()
            .map(str::trim)
            .find_map(|line| number_before(line, label))
            .unwrap_or_default()
    };
    Some(Counts {
        passed: passing,
        failed: line_count(" failing"),
        skipped: line_count(" pending"),
    })
}

/// `go test`: one `ok`, `FAIL` or `?` line per package; `--- PASS`/`--- FAIL`
/// per test with `-v`.
fn go(output: &str) -> Option<Counts> {
    let mut tests = Counts::default();
    let mut packages = Counts::default();
    for line in output.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--- PASS:") {
            tests.passed = tests.passed.saturating_add(1);
        } else if trimmed.starts_with("--- FAIL:") {
            tests.failed = tests.failed.saturating_add(1);
        } else if trimmed.starts_with("--- SKIP:") {
            tests.skipped = tests.skipped.saturating_add(1);
        } else if line.starts_with("ok  \t") || line.starts_with("ok \t") {
            packages.passed = packages.passed.saturating_add(1);
        } else if line.starts_with("FAIL\t") {
            packages.failed = packages.failed.saturating_add(1);
        }
    }
    if tests != Counts::default() {
        Some(tests)
    } else if packages != Counts::default() {
        // Without -v only packages are reported, so the counts are packages.
        Some(packages)
    } else {
        None
    }
}

/// Names of the failing tests a runner lists, in order, without repeats.
///
/// Covers cargo, TAP, pytest, go and the jest and vitest marks. A runner whose
/// failures are not named this way yields an empty list.
#[must_use]
pub fn failures(output: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        let name = trimmed
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... FAILED"))
            .or_else(|| {
                let rest = trimmed.strip_prefix("not ok ")?;
                Some(rest.split_once(" - ")?.1)
            })
            .or_else(|| {
                let rest = trimmed.strip_prefix("FAILED ")?;
                Some(rest.split_once(" - ").map_or(rest, |(name, _)| name))
            })
            .or_else(|| {
                let rest = trimmed.strip_prefix("--- FAIL: ")?;
                Some(rest.rsplit_once(" (").map_or(rest, |(name, _)| name))
            })
            .or_else(|| {
                trimmed
                    .strip_prefix("\u{2715} ")
                    .or_else(|| trimmed.strip_prefix("\u{d7} "))
                    .or_else(|| trimmed.strip_prefix("\u{25cf} "))
            });
        if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty())
            && !names.iter().any(|known| known == name)
        {
            names.push(name.to_owned());
        }
    }
    names
}

/// Fewest unchanged lines worth leaving out of a repeated run.
const MIN_OMITTED_LINES: usize = 8;

/// Shortens a test run against the previous run of the same command.
///
/// Lines the previous log already had are left out, which drops unchanged
/// failure detail; lines that are new, such as a new failure or a changed
/// message, stay in place. A leading line names tests that now pass, still
/// fail, and newly fail. Returns `None` when either log has no test summary
/// or too little would be left out to matter.
#[must_use]
pub fn delta(previous: &str, current: &str) -> Option<String> {
    parse(previous)?;
    parse(current)?;
    let seen: std::collections::HashSet<&str> = previous.lines().map(str::trim_end).collect();
    let mut kept = String::new();
    let mut omitted = 0_usize;
    for line in current.lines() {
        if seen.contains(line.trim_end()) {
            omitted = omitted.saturating_add(usize::from(!line.trim().is_empty()));
        } else {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    if omitted < MIN_OMITTED_LINES {
        return None;
    }
    let before = failures(previous);
    let after = failures(current);
    let list = |names: Vec<&String>, label: &str| {
        if names.is_empty() {
            return None;
        }
        let shown: Vec<&str> = names.iter().take(8).map(|name| name.as_str()).collect();
        let more = names.len().saturating_sub(shown.len());
        let tail = if more > 0 {
            format!(", {more} more")
        } else {
            String::new()
        };
        Some(format!("{label}: {}{tail}", shown.join(", ")))
    };
    let mut parts: Vec<String> = [
        list(
            before.iter().filter(|name| !after.contains(name)).collect(),
            "now passing",
        ),
        list(
            after.iter().filter(|name| before.contains(name)).collect(),
            "still failing",
        ),
        list(
            after.iter().filter(|name| !before.contains(name)).collect(),
            "new failures",
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    parts.push(format!(
        "{omitted} lines identical to the previous run of this command left out"
    ));
    Some(format!(
        "[since the last run: {}]\n{kept}",
        parts.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(passed: u64, failed: u64, skipped: u64) -> Option<Counts> {
        Some(Counts {
            passed,
            failed,
            skipped,
        })
    }

    #[test]
    fn cargo_results_are_summed_over_binaries() {
        let output = "running 3 tests\n...\ntest result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n\nrunning 2 tests\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured\n";
        assert_eq!(parse(output), counts(4, 1, 1));
    }

    #[test]
    fn node_test_reports_its_tap_totals() {
        let output = "TAP version 13\nok 1 - a\nnot ok 2 - b\n1..2\n# tests 2\n# suites 0\n# pass 1\n# fail 1\n# cancelled 0\n# skipped 0\n# todo 0\n# duration_ms 41\n";
        assert_eq!(parse(output), counts(1, 1, 0));
        let spec = "\u{2139} tests 4\n\u{2139} pass 4\n\u{2139} fail 0\n";
        assert_eq!(parse(spec), counts(4, 0, 0));
    }

    #[test]
    fn jest_vitest_pytest_mocha_and_go_summaries_are_read() {
        assert_eq!(
            parse("Test Suites: 1 failed, 1 total\nTests:       1 failed, 3 passed, 4 total\n"),
            counts(3, 1, 0)
        );
        assert_eq!(
            parse(" Test Files  1 failed (1)\n      Tests  1 failed | 3 passed (4)\n"),
            counts(3, 1, 0)
        );
        assert_eq!(
            parse(
                "collected 6 items\n\n=========== 1 failed, 4 passed, 1 skipped in 0.12s ===========\n"
            ),
            counts(4, 1, 1)
        );
        assert_eq!(
            parse("\n  3 passing (12ms)\n  1 failing\n"),
            counts(3, 1, 0)
        );
        assert_eq!(
            parse(
                "=== RUN   TestA\n--- PASS: TestA (0.00s)\n--- FAIL: TestB (0.00s)\nFAIL\nFAIL\tex\t0.01s\n"
            ),
            counts(1, 1, 0)
        );
        assert_eq!(
            parse("ok  \tex/a\t0.01s\nok  \tex/b\t0.02s\n"),
            counts(2, 0, 0)
        );
    }

    #[test]
    fn output_without_a_summary_yields_nothing() {
        assert_eq!(parse("compiling...\nerror: expected `;`\n"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("we passed the test of time"), None);
    }

    #[test]
    fn counts_read_as_a_sentence() {
        assert_eq!(
            counts(3, 1, 0).expect("counts").to_string(),
            "3 passed, 1 failed"
        );
        assert_eq!(
            counts(3, 0, 2).expect("counts").to_string(),
            "3 passed, 0 failed, 2 skipped"
        );
    }

    #[test]
    fn a_repeated_run_keeps_only_what_changed() {
        let detail = "    at frame\n".repeat(10);
        let previous = format!(
            "test a ... FAILED\ntest b ... FAILED\ntest c ... ok\n{detail}\
             test result: FAILED. 1 passed; 2 failed; 0 ignored; finished in 0.10s\n"
        );
        let current = format!(
            "test a ... ok\ntest b ... FAILED\ntest c ... ok\n{detail}\
             assertion failed: new message\n\
             test result: FAILED. 2 passed; 1 failed; 0 ignored; finished in 0.12s\n"
        );
        let delta = delta(&previous, &current).expect("delta");
        assert!(delta.starts_with("[since the last run: now passing: a; still failing: b;"));
        assert!(delta.contains("new message"));
        assert!(delta.contains("2 passed; 1 failed"));
        assert!(!delta.contains("frame"));
        assert!(!delta.contains("test b ... FAILED"));
    }

    #[test]
    fn a_run_with_little_in_common_is_shown_whole() {
        let previous = "test a ... FAILED\ntest result: FAILED. 0 passed; 1 failed;\n";
        let current = "test a ... ok\ntest result: ok. 1 passed; 0 failed;\n";
        assert_eq!(delta(previous, current), None);
        assert_eq!(delta("no summary", current), None);
    }

    #[test]
    fn failing_tests_are_named_for_each_runner() {
        let log = "test m::x ... FAILED\nnot ok 2 - adds\nFAILED t.py::y - assert\n\
                   --- FAIL: TestZ (0.00s)\n  \u{2715} renders\ntest m::x ... FAILED\n";
        assert_eq!(
            failures(log),
            ["m::x", "adds", "t.py::y", "TestZ", "renders"]
        );
    }
}
