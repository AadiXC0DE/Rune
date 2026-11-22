//! The documentation pages, generated from the binary's own reference.
//!
//! `rune reference --json` describes every command, setting, limit, key,
//! provider preset and doctor check from the tables the binary runs on. The
//! pages here are rendered from that document and nothing else, so a page
//! cannot describe something the binary does not do, and a change to the
//! binary that the pages do not reflect fails `cargo xtask verify-web`.
//!
//! Each page is a list of blocks rendered twice: as HTML for the site and as
//! plain text for `llms-full.txt`, so the two cannot say different things.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// Where the site is served.
pub const SITE_URL: &str = "https://rune.heyaadi.com";

/// The heading `llms.txt` lists the generated pages under. Everything before
/// it is kept as written.
const LLMS_SECTION: &str = "## Reference pages";

/// The scripts the site serves as byte copies of their source.
pub const SITE_COPIES: &[(&str, &str)] = &[
    ("scripts/install.sh", "site/install.sh"),
    ("scripts/install.ps1", "site/install.ps1"),
];

/// The colour tokens and type of the landing page, so the pages match it.
const DOCS_CSS: &str = r#"@font-face { font-family: "Plex Mono"; font-style: normal; font-weight: 400; font-display: swap; src: url(../fonts/plex-mono-400.woff2) format("woff2"); }
@font-face { font-family: "Plex Mono"; font-style: normal; font-weight: 600; font-display: swap; src: url(../fonts/plex-mono-600.woff2) format("woff2"); }

:root {
  --bg: #0b0b0a;
  --bg-raised: #121210;
  --line: #23221e;
  --text: #ece8df;
  --dim: #a39d90;
  --accent-ink: #ffb454;
  --gutter: clamp(1.25rem, 4vw, 3rem);
  --width: 1180px;
  color-scheme: dark;
}

@media (prefers-color-scheme: light) {
  :root {
    --bg: #f6f6f3;
    --bg-raised: #ffffff;
    --line: #e3e2dc;
    --text: #1a1916;
    --dim: #57544c;
    --accent-ink: #9a5b00;
    color-scheme: light;
  }
}

*, *::before, *::after { box-sizing: border-box; }
html { -webkit-text-size-adjust: 100%; }
body {
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font-family: "Plex Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 15px;
  line-height: 1.65;
  -webkit-font-smoothing: antialiased;
}
a { color: var(--accent-ink); text-decoration: none; }
a:hover { text-decoration: underline; }
:focus-visible { outline: 2px solid var(--accent-ink); outline-offset: 3px; }
code, pre { font-family: inherit; }
code { color: var(--accent-ink); }
.wrap { max-width: var(--width); margin: 0 auto; padding: 0 var(--gutter); }
.top { border-bottom: 1px solid var(--line); background: var(--bg); }
.top .wrap { display: flex; align-items: center; justify-content: space-between; gap: 1rem; min-height: 3.5rem; flex-wrap: wrap; }
.brand { display: inline-flex; align-items: center; gap: .55rem; color: var(--text); font-weight: 600; }
nav { display: flex; flex-wrap: wrap; gap: .25rem 1.1rem; font-size: .85rem; }
nav a { color: var(--dim); }
nav a:hover, nav a[aria-current="page"] { color: var(--text); }
main { padding-top: 2rem; padding-bottom: 4rem; }
h1 { font-size: 1.6rem; line-height: 1.25; margin: 0 0 1rem; }
h2 { font-size: 1.15rem; margin: 2.5rem 0 .75rem; padding-top: 1rem; border-top: 1px solid var(--line); }
h3 { font-size: 1rem; margin: 1.75rem 0 .5rem; }
p, ul { color: var(--dim); max-width: 80ch; }
ul { padding-left: 1.25rem; }
pre { background: var(--bg-raised); border: 1px solid var(--line); padding: .85rem 1rem; overflow-x: auto; font-size: .85rem; }
.table { overflow-x: auto; margin: .75rem 0 1.25rem; }
table { border-collapse: collapse; font-size: .85rem; min-width: 100%; }
th, td { text-align: left; vertical-align: top; padding: .45rem .75rem; border-bottom: 1px solid var(--line); }
th { color: var(--text); font-weight: 600; white-space: nowrap; }
td { color: var(--dim); }
"#;

/// The mark the landing page shows beside the name.
const MARK: &str = r##"<svg width="20" height="20" viewBox="0 0 32 32" aria-hidden="true"><path d="M7 4V28M7 9.5L18.5 16L7 22.5M20.5 26.5H26.5" fill="none" stroke="#ffb454" stroke-width="3"/></svg>"##;

/// One generated file: where it goes under the repository root, and its bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct Generated {
    /// Path relative to the repository root, with `/` separators.
    pub path: String,
    /// The file's contents.
    pub bytes: Vec<u8>,
}

/// One piece of a page.
enum Block {
    /// A heading of level 1 to 3.
    Heading(u8, String),
    /// A paragraph; backtick spans become code.
    Para(String),
    /// Preformatted text, shown as written.
    Code(String),
    /// A bulleted list; backtick spans become code.
    List(Vec<String>),
    /// A table with a header row; backtick spans become code.
    Table(Vec<&'static str>, Vec<Vec<String>>),
}

/// One page of the documentation.
struct Page {
    /// File name under `site/docs/`.
    file: &'static str,
    /// Title, also the link text.
    title: &'static str,
    /// One line on what the page holds.
    summary: &'static str,
    /// The content.
    blocks: Vec<Block>,
}

/// Escapes text for HTML.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Renders inline text as HTML: escaped, with each backtick span as code.
///
/// An unpaired backtick is kept as a character rather than opening a span
/// that never closes.
pub fn inline(text: &str) -> String {
    let parts: Vec<&str> = text.split('`').collect();
    if parts.len().is_multiple_of(2) {
        return escape(text);
    }
    let mut out = String::new();
    for (index, part) in parts.iter().enumerate() {
        if index % 2 == 1 {
            let _ = write!(out, "<code>{}</code>", escape(part));
        } else {
            out.push_str(&escape(part));
        }
    }
    out
}

/// Returns a string field, or an error naming what was missing.
fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("the reference has no `{field}` text in {value}"))
}

/// Returns an array field, or an error naming what was missing.
fn items<'a>(value: &'a Value, field: &str) -> Result<&'a [Value], String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("the reference has no `{field}` list"))
}

/// Shows an optional value, or `none`.
fn shown(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "none".to_owned(),
        Some(Value::String(text)) if text.is_empty() => "none".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// Spells a flag with its value, as typed.
fn flag_usage(flag: &Value) -> Result<String, String> {
    let name = text(flag, "name")?;
    Ok(match flag.get("value").and_then(Value::as_str) {
        Some(value) => format!("`{name} <{value}>`"),
        None => format!("`{name}`"),
    })
}

/// The command reference.
fn commands_page(reference: &Value) -> Result<Page, String> {
    let mut blocks = vec![
        Block::Heading(1, "Commands".to_owned()),
        Block::Para(
            "Generated from the command table the parser reads, so every entry here is a \
             command the binary accepts."
                .to_owned(),
        ),
        Block::Heading(2, "Global flags".to_owned()),
        Block::Para("Accepted anywhere on the command line.".to_owned()),
    ];
    let mut rows = Vec::new();
    for flag in items(reference, "global_flags")? {
        rows.push(vec![
            flag_usage(flag)?,
            text(flag, "description")?.to_owned(),
        ]);
    }
    blocks.push(Block::Table(vec!["Flag", "Description"], rows));
    let mut group = String::new();
    for command in items(reference, "commands")? {
        let this = text(command, "group")?;
        if this != group {
            this.clone_into(&mut group);
            blocks.push(Block::Heading(2, group.clone()));
        }
        blocks.push(Block::Heading(3, format!("`{}`", text(command, "usage")?)));
        blocks.push(Block::Para(text(command, "summary")?.to_owned()));
        let aliases = items(command, "aliases")?;
        if !aliases.is_empty() {
            let mut list = Vec::new();
            for alias in aliases {
                list.push(format!(
                    "`{}`: {}",
                    text(alias, "name")?,
                    text(alias, "description")?
                ));
            }
            blocks.push(Block::List(list));
        }
        let flags = items(command, "flags")?;
        if !flags.is_empty() {
            let mut rows = Vec::new();
            for flag in flags {
                rows.push(vec![
                    flag_usage(flag)?,
                    text(flag, "description")?.to_owned(),
                ]);
            }
            blocks.push(Block::Table(vec!["Flag", "Description"], rows));
        }
        let notes = text(command, "notes")?;
        if !notes.is_empty() {
            blocks.push(Block::Code(notes.to_owned()));
        }
    }
    Ok(Page {
        file: "commands.html",
        title: "Commands",
        summary: "Every command and flag the binary accepts.",
        blocks,
    })
}

/// The settings and limits.
fn configuration_page(reference: &Value) -> Result<Page, String> {
    let mut rows = Vec::new();
    for setting in items(reference, "settings")? {
        let key = text(setting, "key")?;
        let help = text(setting, "help")?;
        if help.trim().is_empty() {
            return Err(format!("the setting `{key}` has no help line"));
        }
        rows.push(vec![
            format!("`{key}`"),
            text(setting, "type")?.to_owned(),
            shown(setting.get("default")),
            match setting.get("env").and_then(Value::as_str) {
                Some(variable) => format!("`{variable}`"),
                None => "none".to_owned(),
            },
            help.to_owned(),
        ]);
    }
    let mut limits = Vec::new();
    for limit in items(reference, "limits")? {
        let range = limit
            .get("range")
            .ok_or_else(|| "a limit has no range".to_owned())?;
        let upper = match range.get("max") {
            Some(Value::Null) | None => "unbounded".to_owned(),
            Some(max) => max.to_string(),
        };
        limits.push(vec![
            format!("`{}`", text(limit, "name")?),
            shown(limit.get("default")),
            text(limit, "unit")?.to_owned(),
            format!("{} to {upper}", shown(range.get("min"))),
            text(limit, "description")?.to_owned(),
        ]);
    }
    Ok(Page {
        file: "configuration.html",
        title: "Configuration",
        summary: "Every setting with its type, default and variable, and every limit.",
        blocks: vec![
            Block::Heading(1, "Configuration".to_owned()),
            Block::Para(
                "Settings are read from the user configuration file, the project's file, \
                 the environment and the command line. `rune config --explain` shows the \
                 value in force and where it came from, and `rune config set <key> <value>` \
                 writes one."
                    .to_owned(),
            ),
            Block::Heading(2, "Settings".to_owned()),
            Block::Table(
                vec!["Setting", "Type", "Default", "Variable", "Description"],
                rows,
            ),
            Block::Heading(2, "Limits".to_owned()),
            Block::Para(
                "Every limit takes a count, or `off` to remove it. A hard ceiling still \
                 applies to a limit set to `off`."
                    .to_owned(),
            ),
            Block::Table(
                vec!["Limit", "Default", "Unit", "Range", "Description"],
                limits,
            ),
        ],
    })
}

/// One section per provider preset.
fn providers_page(reference: &Value) -> Result<Page, String> {
    let mut blocks = vec![
        Block::Heading(1, "Providers".to_owned()),
        Block::Para(
            "Each preset names an endpoint and the variable its key is read from. \
             `rune connect <name>` connects one; `rune connect` alone chooses from a list. \
             The configuration shown is the smallest file that selects the preset."
                .to_owned(),
        ),
    ];
    for preset in items(reference, "presets")? {
        let name = text(preset, "name")?;
        blocks.push(Block::Heading(2, name.to_owned()));
        blocks.push(Block::Para(format!(
            "{}. Dialect: `{}`. {}.",
            text(preset, "label")?,
            text(preset, "dialect")?,
            text(preset, "notes")?
        )));
        blocks.push(Block::Code(format!("rune connect {name}")));
        let mut facts = Vec::new();
        match preset.get("base_url").and_then(Value::as_str) {
            Some(url) => facts.push(format!("Endpoint: `{url}`.")),
            None => facts.push("Endpoint: none built in; set `base_url`.".to_owned()),
        }
        if let Some(variable) = preset.get("key_variable").and_then(Value::as_str) {
            facts.push(format!("Key variable: `{variable}`."));
        }
        if preset.get("anonymous").and_then(Value::as_bool) == Some(true) {
            facts.push("No key: the server is reached on this machine only.".to_owned());
        }
        blocks.push(Block::List(facts));
        blocks.push(Block::Code(text(preset, "config")?.to_owned()));
    }
    Ok(Page {
        file: "providers.html",
        title: "Providers",
        summary: "How to connect each provider preset.",
        blocks,
    })
}

/// The default key bindings.
fn keys_page(reference: &Value) -> Result<Page, String> {
    let mut rows = Vec::new();
    for key in items(reference, "keys")? {
        let defaults: Vec<String> = items(key, "default")?
            .iter()
            .filter_map(Value::as_str)
            .map(|key| format!("`{key}`"))
            .collect();
        rows.push(vec![
            format!("`{}`", text(key, "action")?),
            text(key, "context")?.to_owned(),
            if defaults.is_empty() {
                "none".to_owned()
            } else {
                defaults.join(", ")
            },
            text(key, "summary")?.to_owned(),
        ]);
    }
    Ok(Page {
        file: "keys.html",
        title: "Key bindings",
        summary: "Every action and the keys it has by default.",
        blocks: vec![
            Block::Heading(1, "Key bindings".to_owned()),
            Block::Para(
                "The keys each action has when the key file does not name it. `rune keys` \
                 lists the bindings in force."
                    .to_owned(),
            ),
            Block::Table(vec!["Action", "Where", "Keys", "Does"], rows),
        ],
    })
}

/// Every doctor check.
fn troubleshooting_page(reference: &Value) -> Result<Page, String> {
    let mut rows = Vec::new();
    for check in items(reference, "doctor")? {
        rows.push(vec![
            text(check, "check")?.to_owned(),
            text(check, "looks_at")?.to_owned(),
            text(check, "fix")?.to_owned(),
        ]);
    }
    Ok(Page {
        file: "troubleshooting.html",
        title: "Troubleshooting",
        summary: "What `rune doctor` checks and how to fix each problem.",
        blocks: vec![
            Block::Heading(1, "Troubleshooting".to_owned()),
            Block::Para(
                "`rune doctor` runs these checks in this order and prints `ok`, `warning`, \
                 `problem` or `skipped` for each, with a fix for anything wrong."
                    .to_owned(),
            ),
            Block::Table(vec!["Check", "Looks at", "Fix"], rows),
        ],
    })
}

/// Formats a share as a percentage, or `n/a`.
fn percent(value: Option<&Value>) -> String {
    value.and_then(Value::as_f64).map_or_else(
        || "n/a".to_owned(),
        |share| format!("{:.1}%", share * 100.0),
    )
}

/// Formats a number with two decimals, or `n/a`.
fn decimal(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_f64)
        .map_or_else(|| "n/a".to_owned(), |number| format!("{number:.2}"))
}

/// The published benchmark reports, one table per run.
fn benchmarks_page(root: &Path) -> Result<Page, String> {
    let published = root.join("bench").join("published");
    let mut runs: Vec<PathBuf> = match std::fs::read_dir(&published) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("report.json"))
            .filter(|path| path.is_file())
            .collect(),
        Err(_) => Vec::new(),
    };
    runs.sort();
    let mut blocks = vec![Block::Heading(1, "Benchmarks".to_owned())];
    if runs.is_empty() {
        blocks.push(Block::Para("No results are published yet.".to_owned()));
    }
    for path in runs {
        let shown_path = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let report: Value = std::fs::read_to_string(&path)
            .map_err(|err| format!("could not read {shown_path}: {err}"))
            .and_then(|body| {
                serde_json::from_str(&body)
                    .map_err(|err| format!("{shown_path} is not valid JSON: {err}"))
            })?;
        let run = path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        blocks.push(Block::Heading(2, run));
        if let Some(models) = report
            .get("header")
            .and_then(|header| header.get("models"))
            .and_then(Value::as_array)
        {
            let models: Vec<&str> = models.iter().filter_map(Value::as_str).collect();
            blocks.push(Block::Para(format!("Models: {}.", models.join(", "))));
        }
        let mut rows = Vec::new();
        for harness in items(&report, "harnesses").map_err(|err| format!("{shown_path}: {err}"))? {
            rows.push(vec![
                text(harness, "harness")?.to_owned(),
                shown(harness.get("runs")),
                percent(harness.get("pass_at_1")),
                percent(harness.get("verify_rate")),
                decimal(harness.get("cost_per_solved")),
                decimal(harness.get("median_wall_s")),
            ]);
        }
        blocks.push(Block::Table(
            vec![
                "Harness",
                "Runs",
                "Pass@1",
                "Verify passed",
                "Cost per solved (USD)",
                "Median wall (s)",
            ],
            rows,
        ));
        blocks.push(Block::Para(format!("Source: `{shown_path}`.")));
    }
    Ok(Page {
        file: "benchmarks.html",
        title: "Benchmarks",
        summary: "Published benchmark results.",
        blocks,
    })
}

/// The page that lists the others.
fn index_page(pages: &[Page], version: &str) -> Page {
    let list = pages
        .iter()
        .map(|page| format!("[{}]({}): {}", page.title, page.file, page.summary))
        .collect();
    Page {
        file: "index.html",
        title: "Documentation",
        summary: "Reference pages generated from the binary.",
        blocks: vec![
            Block::Heading(1, "Documentation".to_owned()),
            Block::Para(format!(
                "Generated from `rune reference --json` of rune {version}, so these pages \
                 describe what the binary does."
            )),
            Block::List(list),
        ],
    }
}

/// Renders a list item, turning a leading `[title](file)` into a link.
fn list_item_html(item: &str) -> String {
    if let Some(rest) = item.strip_prefix('[')
        && let Some((title, rest)) = rest.split_once("](")
        && let Some((target, rest)) = rest.split_once(')')
    {
        return format!(
            "<a href=\"{}\">{}</a>{}",
            escape(target),
            escape(title),
            inline(rest)
        );
    }
    inline(item)
}

/// Renders a page as HTML with the site's header.
fn page_html(page: &Page, pages: &[(&str, &str)]) -> String {
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = writeln!(out, "<title>{} · Rune</title>", escape(page.title));
    let _ = writeln!(
        out,
        "<meta name=\"description\" content=\"{}\">",
        escape(&page.summary.replace('`', ""))
    );
    let _ = writeln!(
        out,
        "<link rel=\"canonical\" href=\"{SITE_URL}/docs/{}\">",
        page.file
    );
    out.push_str("<link rel=\"stylesheet\" href=\"docs.css\">\n</head>\n<body>\n\n");
    out.push_str("<header class=\"top\">\n  <div class=\"wrap\">\n");
    let _ = writeln!(
        out,
        "    <a class=\"brand\" href=\"/\" aria-label=\"Rune home\">{MARK} rune</a>"
    );
    out.push_str("    <nav>\n");
    for (file, title) in pages {
        let current = if *file == page.file {
            " aria-current=\"page\""
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "      <a href=\"{file}\"{current}>{}</a>",
            escape(&title.to_lowercase())
        );
    }
    out.push_str("    </nav>\n  </div>\n</header>\n\n<main class=\"wrap\">\n");
    for block in &page.blocks {
        match block {
            Block::Heading(level, text) => {
                let _ = writeln!(out, "<h{level}>{}</h{level}>", inline(text));
            }
            Block::Para(text) => {
                let _ = writeln!(out, "<p>{}</p>", inline(text));
            }
            Block::Code(text) => {
                let _ = writeln!(out, "<pre>{}</pre>", escape(text.trim_end()));
            }
            Block::List(items) => {
                out.push_str("<ul>\n");
                for item in items {
                    let _ = writeln!(out, "  <li>{}</li>", list_item_html(item));
                }
                out.push_str("</ul>\n");
            }
            Block::Table(header, rows) => {
                out.push_str("<div class=\"table\"><table>\n<thead><tr>");
                for cell in header {
                    let _ = write!(out, "<th>{}</th>", escape(cell));
                }
                out.push_str("</tr></thead>\n<tbody>\n");
                for row in rows {
                    out.push_str("<tr>");
                    for cell in row {
                        let _ = write!(out, "<td>{}</td>", inline(cell));
                    }
                    out.push_str("</tr>\n");
                }
                out.push_str("</tbody>\n</table></div>\n");
            }
        }
    }
    out.push_str("</main>\n\n</body>\n</html>\n");
    out
}

/// Renders a page as plain text, for `llms-full.txt`.
fn page_text(page: &Page) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "<!-- {SITE_URL}/docs/{} -->\n", page.file);
    for block in &page.blocks {
        match block {
            Block::Heading(level, text) => {
                let _ = writeln!(out, "{} {text}\n", "#".repeat(usize::from(*level)));
            }
            Block::Para(text) => {
                let _ = writeln!(out, "{text}\n");
            }
            Block::Code(text) => {
                let _ = writeln!(out, "```text\n{}\n```\n", text.trim_end());
            }
            Block::List(items) => {
                for item in items {
                    let _ = writeln!(out, "- {item}");
                }
                out.push('\n');
            }
            Block::Table(header, rows) => {
                let _ = writeln!(out, "| {} |", header.join(" | "));
                let _ = writeln!(out, "|{}", "---|".repeat(header.len()));
                for row in rows {
                    let cells: Vec<String> =
                        row.iter().map(|cell| cell.replace('|', "\\|")).collect();
                    let _ = writeln!(out, "| {} |", cells.join(" | "));
                }
                out.push('\n');
            }
        }
    }
    out
}

/// Rewrites `llms.txt`: the text before the generated section as written, then
/// one link per page.
fn llms_index(current: &str, pages: &[Page]) -> String {
    let kept = current
        .split_once(&format!("\n{LLMS_SECTION}\n"))
        .map_or(current, |(before, _)| before)
        .trim_end();
    let mut out = format!("{kept}\n\n{LLMS_SECTION}\n\n");
    for page in pages {
        let _ = writeln!(
            out,
            "- [{}]({SITE_URL}/docs/{}): {}",
            page.title,
            page.file,
            page.summary.replace('`', "")
        );
    }
    let _ = writeln!(
        out,
        "- [Everything in one file]({SITE_URL}/llms-full.txt): The plain text of every page above."
    );
    out
}

/// Lists the home page and every documentation page.
fn sitemap(pages: &[Page]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    let _ = writeln!(out, "  <url>\n    <loc>{SITE_URL}/</loc>\n  </url>");
    for page in pages {
        let _ = writeln!(
            out,
            "  <url>\n    <loc>{SITE_URL}/docs/{}</loc>\n  </url>",
            page.file
        );
    }
    out.push_str("</urlset>\n");
    out
}

/// Generates every site file from the reference and the repository at `root`.
///
/// Nothing is written; the caller decides whether to write or compare. The
/// output depends only on its inputs, in a fixed order, and carries no date.
pub fn generate(root: &Path, reference: &Value) -> Result<Vec<Generated>, String> {
    let version = text(reference, "version")?;
    let mut pages = vec![
        commands_page(reference)?,
        configuration_page(reference)?,
        providers_page(reference)?,
        keys_page(reference)?,
        troubleshooting_page(reference)?,
        benchmarks_page(root)?,
    ];
    pages.insert(0, index_page(&pages, version));
    let nav: Vec<(&str, &str)> = pages.iter().map(|page| (page.file, page.title)).collect();

    let mut files = Vec::new();
    for page in &pages {
        files.push(Generated {
            path: format!("site/docs/{}", page.file),
            bytes: page_html(page, &nav).into_bytes(),
        });
    }
    files.push(Generated {
        path: "site/docs/docs.css".to_owned(),
        bytes: DOCS_CSS.as_bytes().to_vec(),
    });
    let current = std::fs::read_to_string(root.join("site").join("llms.txt"))
        .map_err(|err| format!("could not read site/llms.txt: {err}"))?
        .replace("\r\n", "\n");
    files.push(Generated {
        path: "site/llms.txt".to_owned(),
        bytes: llms_index(&current, &pages).into_bytes(),
    });
    let full: String = pages.iter().map(page_text).collect();
    files.push(Generated {
        path: "site/llms-full.txt".to_owned(),
        bytes: full.into_bytes(),
    });
    files.push(Generated {
        path: "site/sitemap.xml".to_owned(),
        bytes: sitemap(&pages).into_bytes(),
    });
    for (source, copy) in SITE_COPIES {
        let bytes = std::fs::read(root.join(source))
            .map_err(|err| format!("could not read {source}: {err}"))?;
        files.push(Generated {
            path: (*copy).to_owned(),
            bytes,
        });
    }
    Ok(files)
}

/// Builds the binary and returns its `rune reference --json`.
///
/// The binary runs with an empty home and no configuration, so the defaults it
/// reports are the defaults, not this machine's settings.
pub fn reference_json(root: &Path, target: &Path) -> Result<Value, String> {
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
        .args(["build", "--quiet", "-p", "rune"])
        .current_dir(root)
        .status()
        .map_err(|err| format!("could not run cargo build: {err}"))?;
    if !status.success() {
        return Err("cargo build -p rune failed".to_owned());
    }
    let binary = target
        .join("debug")
        .join(if cfg!(windows) { "rune.exe" } else { "rune" });
    let home = std::env::temp_dir().join(format!("rune-site-{}", std::process::id()));
    std::fs::create_dir_all(&home)
        .map_err(|err| format!("could not create {}: {err}", home.display()))?;
    let mut command = Command::new(&binary);
    command
        .args(["reference", "--json"])
        .env_clear()
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("RUNE_HOME", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("XDG_STATE_HOME", &home)
        .env("XDG_DATA_HOME", &home)
        .env("APPDATA", &home)
        .env("LOCALAPPDATA", &home);
    for name in ["SystemRoot", "windir"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let output = command
        .output()
        .map_err(|err| format!("could not run {}: {err}", binary.display()));
    let _ = std::fs::remove_dir_all(&home);
    let output = output?;
    if !output.status.success() {
        return Err(format!(
            "rune reference --json failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("rune reference --json printed something other than JSON: {err}"))
}

/// Writes the generated files, and removes any page under `site/docs/` that is
/// no longer generated.
pub fn write(root: &Path, files: &[Generated]) -> Result<(), String> {
    let docs = root.join("site").join("docs");
    std::fs::create_dir_all(&docs)
        .map_err(|err| format!("could not create {}: {err}", docs.display()))?;
    for stale in stale_pages(root, files) {
        std::fs::remove_file(root.join(&stale))
            .map_err(|err| format!("could not remove {stale}: {err}"))?;
    }
    for file in files {
        let path = root.join(&file.path);
        if std::fs::read(&path).ok().as_deref() == Some(file.bytes.as_slice()) {
            continue;
        }
        std::fs::write(&path, &file.bytes)
            .map_err(|err| format!("could not write {}: {err}", file.path))?;
    }
    Ok(())
}

/// Returns text with CRLF turned into line feeds, and other bytes unchanged.
///
/// A checkout that converts line endings holds the same text, so it is not a
/// stale page.
fn line_feeds(bytes: Vec<u8>) -> Vec<u8> {
    match String::from_utf8(bytes) {
        Ok(text) => text.replace("\r\n", "\n").into_bytes(),
        Err(err) => err.into_bytes(),
    }
}

/// Returns files under `site/docs/` that the generator does not produce.
fn stale_pages(root: &Path, files: &[Generated]) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join("site").join("docs")) else {
        return Vec::new();
    };
    let mut stale: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| format!("site/docs/{}", entry.file_name().to_string_lossy()))
        .filter(|path| !files.iter().any(|file| &file.path == path))
        .collect();
    stale.sort();
    stale
}

/// Fails, naming the first, when a committed file differs from what the
/// generator produces, or when a page is committed that it no longer makes.
pub fn verify(root: &Path, files: &[Generated]) -> Result<(), String> {
    let regenerate = "run cargo xtask site and commit the result";
    for file in files {
        let committed = std::fs::read(root.join(&file.path)).ok().map(line_feeds);
        if committed != Some(line_feeds(file.bytes.clone())) {
            return Err(format!("{} is stale; {regenerate}", file.path));
        }
    }
    if let Some(stale) = stale_pages(root, files).first() {
        return Err(format!("{stale} is no longer generated; {regenerate}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::*;

    /// A reference with one entry in every section.
    fn reference() -> Value {
        serde_json::json!({
            "version": "9.9.9",
            "commands": [{
                "name": "copy",
                "group": "Run",
                "usage": "rune copy <path>",
                "summary": "Copy <path> & keep `it`",
                "aliases": [{"name": "cp", "description": "Same as `rune copy`."}],
                "flags": [{"name": "--to", "value": "dir", "description": "Where to copy."}],
                "notes": "",
            }],
            "global_flags": [{"name": "--json", "value": null, "description": "Emit JSON."}],
            "settings": [{
                "key": "model",
                "type": "string",
                "default": "",
                "help": "Model used for the active provider.",
                "env": "RUNE_MODEL",
            }],
            "limits": [{
                "name": "max_steps",
                "default": 100,
                "unit": "steps",
                "range": {"min": 1, "max": null},
                "description": "Steps in one turn.",
            }],
            "keys": [{
                "action": "submit",
                "default": ["enter"],
                "context": "editor",
                "summary": "send the draft",
            }],
            "presets": [{
                "name": "example",
                "label": "Example API",
                "dialect": "chat_completions",
                "base_url": null,
                "key_variable": "EXAMPLE_API_KEY",
                "anonymous": false,
                "notes": "Gateways; API key",
                "config": "provider = \"example\"\nbase_url = \"<endpoint>\"\n\n[models]\nexample = \"<model id>\"\n",
            }],
            "doctor": [{"check": "git", "looks_at": "git", "fix": "install git"}],
        })
    }

    /// A repository root holding the files the generator reads.
    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, body) in [
            (
                "site/llms.txt",
                "# Rune\n\n> One line.\n\n## Website\n\n- [Home](/): Home.\n",
            ),
            ("scripts/install.sh", "#!/bin/sh\necho install\n"),
            ("scripts/install.ps1", "Write-Output install\n"),
        ] {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, body).expect("write");
        }
        dir
    }

    /// Returns one generated file's text.
    fn file<'a>(files: &'a [Generated], path: &str) -> &'a str {
        let found = files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("{path} was not generated"));
        std::str::from_utf8(&found.bytes).expect("utf8")
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(
            inline("a <b> & `c<d>` 'e' \"f\""),
            "a &lt;b&gt; &amp; <code>c&lt;d&gt;</code> &#39;e&#39; &quot;f&quot;"
        );
        // An unpaired backtick is a character, not a span that never closes.
        assert_eq!(inline("a ` b"), "a ` b");
        let root = root();
        let files = generate(root.path(), &reference()).expect("generate");
        let commands = file(&files, "site/docs/commands.html");
        assert!(
            commands.contains("Copy &lt;path&gt; &amp; keep <code>it</code>"),
            "{commands}"
        );
        assert!(!commands.contains("<path>"), "{commands}");
    }

    #[test]
    fn pages_are_deterministic() {
        let first = root();
        let second = root();
        let a = generate(first.path(), &reference()).expect("generate");
        let b = generate(second.path(), &reference()).expect("generate");
        assert_eq!(a, b);
        write(first.path(), &a).expect("write");
        write(second.path(), &b).expect("write");
        for file in &a {
            let left = std::fs::read(first.path().join(&file.path)).expect("first");
            let right = std::fs::read(second.path().join(&file.path)).expect("second");
            assert_eq!(left, right, "{}", file.path);
        }
        // Generating again from what was written changes nothing, llms.txt
        // included: its generated section replaces the previous one.
        let again = generate(first.path(), &reference()).expect("generate");
        assert_eq!(again, a);
        let llms = file(&again, "site/llms.txt");
        assert_eq!(llms.matches(LLMS_SECTION).count(), 1, "{llms}");
        assert!(llms.starts_with("# Rune\n\n> One line."), "{llms}");
    }

    #[test]
    fn every_page_has_the_header_and_a_link_home() {
        let root = root();
        let files = generate(root.path(), &reference()).expect("generate");
        let pages: Vec<&Generated> = files
            .iter()
            .filter(|file| file.path.to_ascii_lowercase().ends_with(".html"))
            .collect();
        assert_eq!(pages.len(), 7);
        for page in pages {
            let html = std::str::from_utf8(&page.bytes).expect("utf8");
            assert!(html.contains("<header class=\"top\">"), "{}", page.path);
            assert!(html.contains("href=\"/\""), "{}", page.path);
            assert!(!html.contains("<script"), "{}", page.path);
        }
        let sitemap = file(&files, "site/sitemap.xml");
        assert!(sitemap.contains("/docs/providers.html"), "{sitemap}");
        let full = file(&files, "site/llms-full.txt");
        assert!(full.contains("rune connect example"), "{full}");
        assert!(
            file(&files, "site/docs/benchmarks.html").contains("No results are published yet.")
        );
    }

    #[test]
    fn verify_reports_the_stale_file() {
        let root = root();
        let files = generate(root.path(), &reference()).expect("generate");
        write(root.path(), &files).expect("write");
        assert_eq!(verify(root.path(), &files), Ok(()));

        // A checkout with CRLF line endings holds the same pages.
        let keys = root.path().join("site/docs/keys.html");
        let text = std::fs::read_to_string(&keys).expect("read");
        std::fs::write(&keys, text.replace('\n', "\r\n")).expect("write");
        assert_eq!(verify(root.path(), &files), Ok(()));

        std::fs::write(&keys, "edited by hand\n").expect("write");
        assert_eq!(
            verify(root.path(), &files),
            Err(
                "site/docs/keys.html is stale; run cargo xtask site and commit the result"
                    .to_owned()
            )
        );
        write(root.path(), &files).expect("write");
        std::fs::write(root.path().join("site/docs/old.html"), "gone\n").expect("write");
        assert_eq!(
            verify(root.path(), &files),
            Err(
                "site/docs/old.html is no longer generated; run cargo xtask site and commit the result"
                    .to_owned()
            )
        );
        // Writing removes the page that is no longer generated.
        write(root.path(), &files).expect("write");
        assert!(!root.path().join("site/docs/old.html").exists());
        assert_eq!(verify(root.path(), &files), Ok(()));
    }

    #[test]
    fn a_setting_without_help_fails() {
        let mut reference = reference();
        reference["settings"][0]["help"] = Value::from(" ");
        let root = root();
        assert_eq!(
            generate(root.path(), &reference),
            Err("the setting `model` has no help line".to_owned())
        );
    }

    #[test]
    fn a_published_report_that_is_not_json_is_named() {
        let root = root();
        let run = root.path().join("bench/published/2026-11-30-suite");
        std::fs::create_dir_all(&run).expect("dir");
        std::fs::write(run.join("report.json"), "{ not json").expect("write");
        let error = generate(root.path(), &reference()).expect_err("invalid report");
        assert!(
            error.starts_with("bench/published/2026-11-30-suite/report.json is not valid JSON"),
            "{error}"
        );
    }

    #[test]
    fn a_published_report_becomes_a_table() {
        let root = root();
        let run = root.path().join("bench/published/2026-11-30-suite");
        std::fs::create_dir_all(&run).expect("dir");
        let report = serde_json::json!({
            "header": {"models": ["model-a"]},
            "harnesses": [{
                "harness": "rune",
                "runs": 10,
                "pass_at_1": 0.5,
                "verify_rate": 0.6,
                "cost_per_solved": 0.126,
                "median_wall_s": 42.0,
            }],
        });
        std::fs::write(run.join("report.json"), report.to_string()).expect("write");
        let files = generate(root.path(), &reference()).expect("generate");
        let page = file(&files, "site/docs/benchmarks.html");
        assert!(page.contains("<h2>2026-11-30-suite</h2>"), "{page}");
        assert!(
            page.contains("<td>50.0%</td><td>60.0%</td><td>0.13</td>"),
            "{page}"
        );
        assert!(!page.contains("No results are published yet."), "{page}");
    }
}
