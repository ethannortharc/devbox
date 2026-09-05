//! The `[mcp.<name>]` table in `devbox.toml`.
//!
//! Reading is plain serde through [`DevboxConfig`]. Writing is not: `devbox
//! init` generates a *commented* file and users edit it by hand, while
//! [`DevboxConfig::save`] round-trips through `toml::to_string_pretty`, which
//! re-emits the document from the parsed value and drops every comment and all
//! original ordering with it. `devbox mcp add` is not worth a user's comments,
//! so it does not use that path: it edits the text, appending or removing
//! exactly one table and leaving every other byte of the file alone.
//!
//! The edit is text surgery, so it is checked rather than trusted: both
//! [`add_entry`] and [`remove_entry`] re-parse their own output and refuse to
//! return it unless the resulting config differs from the original in exactly
//! the one entry that was meant to change.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::policy::Posture;
use crate::sandbox::config::DevboxConfig;

/// One registered MCP server.
///
/// `box` is the box the *server* runs in, not the box a command acts on — it
/// is a property of the registration, which is why `devbox mcp add` spells it
/// as a `--box` flag while every other command takes its box as a positional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpEntry {
    /// The command and its arguments, as an argv vector — never a shell line.
    pub command: Vec<String>,

    /// Which box to run it in. `None` means the current project's box.
    #[serde(rename = "box", default, skip_serializing_if = "Option::is_none")]
    pub box_name: Option<String>,

    /// Egress posture for the duration of the run (§7.2). Box-granular (N2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posture: Option<Posture>,

    /// Extra environment for the guest process.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

/// The `[mcp.*]` section of a config, keyed by server name.
pub type McpTable = BTreeMap<String, McpEntry>;

/// Names we accept.
///
/// The name is a bare TOML key in a file we edit as text, and it reaches a
/// guest path (`/tmp/devbox-mcp-<name>-…`) and a host log path
/// (`~/.devbox/mcp/<name>.log`). One character class keeps all three
/// unambiguous, and rejecting up front beats discovering the quoting problem
/// when the table cannot be found again.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("an MCP server name cannot be empty");
    }
    if name.len() > 64 {
        bail!("MCP server name '{name}' is longer than 64 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "invalid MCP server name '{name}': use letters, digits, '-' and '_' only \
             (the name is a TOML key, a log filename, and a guest path)"
        );
    }
    Ok(())
}

/// Render one `[mcp.<name>]` table.
///
/// Values only, then the inline `env` table: the whole entry is one contiguous
/// block of lines, which is what makes [`remove_entry`]'s "delete to the next
/// header" rule exact rather than approximate.
fn render(name: &str, entry: &McpEntry) -> String {
    let mut out = format!("[mcp.{name}]\n");
    out.push_str(&format!(
        "command = {}\n",
        toml_string_array(&entry.command)
    ));
    if let Some(box_name) = &entry.box_name {
        out.push_str(&format!("box = {}\n", toml_string(box_name)));
    }
    if let Some(posture) = entry.posture {
        out.push_str(&format!("posture = {}\n", toml_string(posture.as_str())));
    }
    if !entry.env.is_empty() {
        let mut keys: Vec<&String> = entry.env.keys().collect();
        keys.sort();
        let pairs: Vec<String> = keys
            .iter()
            .map(|k| format!("{} = {}", toml_key(k), toml_string(&entry.env[*k])))
            .collect();
        out.push_str(&format!("env = {{ {} }}\n", pairs.join(", ")));
    }
    out
}

/// A TOML basic string. Escapes what the spec requires and nothing else.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn toml_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        key.to_string()
    } else {
        toml_string(key)
    }
}

fn toml_string_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|v| toml_string(v)).collect();
    format!("[{}]", items.join(", "))
}

/// Where the `[mcp.<name>]` block starts and ends in `text`, as a line range.
///
/// A TOML table runs from its header to the next header or to end of file.
/// Only a line whose *first* non-whitespace character is `[` starts a table,
/// and only outside a multi-line string — which is why this tracks `'''` and
/// `"""` rather than scanning for `[` alone. A `devbox.toml` with a multi-line
/// string containing what looks like a header is unlikely; silently deleting
/// half the file when it happens is not acceptable, and the check is four
/// lines.
fn block_of(text: &str, name: &str) -> Option<(usize, usize)> {
    let header = format!("[mcp.{name}]");
    let mut start = None;
    let mut in_multiline: Option<&'static str> = None;

    for (index, line) in text.lines().enumerate() {
        if let Some(delimiter) = in_multiline {
            if line.contains(delimiter) {
                in_multiline = None;
            }
            continue;
        }
        // A line that opens a multi-line string without closing it swallows
        // the lines that follow.
        for delimiter in ["'''", "\"\"\""] {
            if line.matches(delimiter).count() % 2 == 1 {
                in_multiline = Some(delimiter);
            }
        }
        if in_multiline.is_some() {
            continue;
        }

        let trimmed = line.trim();
        if !trimmed.starts_with('[') {
            continue;
        }
        match start {
            None if trimmed == header => start = Some(index),
            None => {}
            Some(begin) => return Some((begin, index)),
        }
    }
    start.map(|begin| (begin, text.lines().count()))
}

/// Remove lines `[start, end)` plus the blank lines they leave behind.
fn without_lines(text: &str, start: usize, end: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    kept.extend_from_slice(&lines[..start]);
    kept.extend_from_slice(&lines[end.min(lines.len())..]);
    // The block was preceded by a blank separator line; leaving it behind
    // accumulates one blank line per removal.
    while kept.len() > start && start > 0 && kept[start - 1].trim().is_empty() {
        kept.remove(start - 1);
    }
    let mut out = kept.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// The file text with `[mcp.<name>]` set to `entry`, comments preserved.
///
/// An existing entry of that name is removed first, so this is an upsert and
/// the new table always lands at the end of the file — next to the other
/// entries `mcp add` wrote, and after whatever the user has arranged above.
pub fn add_entry(text: &str, name: &str, entry: &McpEntry) -> Result<String> {
    validate_name(name)?;
    let before: DevboxConfig = toml::from_str(text).map_err(|e| {
        anyhow::anyhow!("devbox.toml does not parse, so it will not be edited: {e}")
    })?;

    let stripped = match block_of(text, name) {
        Some((start, end)) => without_lines(text, start, end),
        None => text.to_string(),
    };

    let mut out = stripped;
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(&render(name, entry));

    let after: DevboxConfig = toml::from_str(&out).map_err(|e| {
        anyhow::anyhow!("editing devbox.toml produced a file that does not parse: {e}")
    })?;
    verify_only_change(&before, &after, name, Some(entry))?;
    Ok(out)
}

/// The file text without `[mcp.<name>]`, or `None` if there was no such entry.
pub fn remove_entry(text: &str, name: &str) -> Result<Option<String>> {
    validate_name(name)?;
    let before: DevboxConfig = toml::from_str(text).map_err(|e| {
        anyhow::anyhow!("devbox.toml does not parse, so it will not be edited: {e}")
    })?;
    if !before.mcp.contains_key(name) {
        return Ok(None);
    }
    let Some((start, end)) = block_of(text, name) else {
        bail!(
            "'{name}' is registered in devbox.toml but not as a `[mcp.{name}]` table \
             (an inline or dotted form), so it cannot be removed without rewriting the \
             file and losing its comments; delete the entry by hand"
        );
    };
    let out = without_lines(text, start, end);
    let after: DevboxConfig = toml::from_str(&out).map_err(|e| {
        anyhow::anyhow!("editing devbox.toml produced a file that does not parse: {e}")
    })?;
    verify_only_change(&before, &after, name, None)?;
    Ok(Some(out))
}

/// Refuse an edit that changed anything but the one entry.
///
/// Text surgery on a format with as many spellings as TOML is only safe if the
/// result is checked. Comparing the *parsed* configs is the check: everything
/// the file means, except `[mcp.<name>]`, has to be identical.
fn verify_only_change(
    before: &DevboxConfig,
    after: &DevboxConfig,
    name: &str,
    expected: Option<&McpEntry>,
) -> Result<()> {
    if after.mcp.get(name) != expected {
        bail!("editing devbox.toml did not produce the requested `[mcp.{name}]` entry");
    }
    let mut before_rest = before.mcp.clone();
    let mut after_rest = after.mcp.clone();
    before_rest.remove(name);
    after_rest.remove(name);
    if before_rest != after_rest {
        bail!("editing `[mcp.{name}]` in devbox.toml would have changed another MCP entry");
    }

    // Everything outside `[mcp]`, compared as TOML values so this keeps
    // covering sections added to `DevboxConfig` after today.
    let strip = |config: &DevboxConfig| -> Result<toml::Value> {
        let mut value = toml::Value::try_from(config)?;
        if let Some(table) = value.as_table_mut() {
            table.remove("mcp");
        }
        Ok(value)
    };
    if strip(before)? != strip(after)? {
        bail!("editing `[mcp.{name}]` in devbox.toml would have changed the rest of the file");
    }
    Ok(())
}

/// The `devbox.toml` a project-scoped `mcp` command reads and writes.
pub fn config_path(project_dir: &Path) -> std::path::PathBuf {
    project_dir.join("devbox.toml")
}

/// Read the registry, treating a missing file as an empty one.
///
/// Fallible on a *malformed* file: `mcp run` resolving to "no such server"
/// because `devbox.toml` has a typo in it would send the user looking for a
/// registration they can see with their own eyes.
pub fn load(project_dir: &Path) -> Result<McpTable> {
    let path = config_path(project_dir);
    if !path.exists() {
        return Ok(McpTable::new());
    }
    Ok(DevboxConfig::load(&path)?.mcp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(command: &[&str]) -> McpEntry {
        McpEntry {
            command: command.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    const COMMENTED: &str = "\
# devbox.toml
# hand-written, with opinions

[sandbox]
runtime = \"lima\"   # the comment that keeps getting deleted
mount_mode = \"overlay\"

[languages]
python = true

[policy]
egress = \"allowlist\"
allow = [\"pypi.org\"]
";

    #[test]
    fn adding_an_entry_keeps_every_comment_and_every_other_table() {
        let out = add_entry(COMMENTED, "fetch", &entry(&["uvx", "mcp-server-fetch"])).unwrap();
        assert!(
            out.starts_with(COMMENTED),
            "the original text was rewritten:\n{out}"
        );
        assert!(out.contains("# the comment that keeps getting deleted"));
        assert!(out.contains("[mcp.fetch]"));
        assert!(out.contains("command = [\"uvx\", \"mcp-server-fetch\"]"));

        let config: DevboxConfig = toml::from_str(&out).unwrap();
        assert_eq!(config.sandbox.runtime, "lima");
        assert!(config.languages.python);
        assert_eq!(config.policy.egress, Posture::Allowlist);
        assert_eq!(config.mcp["fetch"].command, ["uvx", "mcp-server-fetch"]);
    }

    /// The reason this module exists at all.
    #[test]
    fn the_serde_writer_would_have_lost_the_comments() {
        let config: DevboxConfig = toml::from_str(COMMENTED).unwrap();
        let round_tripped = toml::to_string_pretty(&config).unwrap();
        assert!(
            !round_tripped.contains("# the comment that keeps getting deleted"),
            "if `toml` learns to keep comments, `add_entry` can go back to `save`"
        );
    }

    #[test]
    fn add_records_box_posture_and_env() {
        let mut e = entry(&["node", "server.js"]);
        e.box_name = Some("mcp-tools".into());
        e.posture = Some(Posture::MirrorOnly);
        e.env
            .insert("TOKEN_NAME".into(), "a \"quoted\" value".into());
        let out = add_entry("", "srv", &e).unwrap();
        let config: DevboxConfig = toml::from_str(&out).unwrap();
        assert_eq!(config.mcp["srv"], e);
        assert!(out.contains("posture = \"mirror-only\""), "{out}");
    }

    #[test]
    fn adding_the_same_name_twice_replaces_it_rather_than_duplicating_the_key() {
        let first = add_entry(COMMENTED, "fetch", &entry(&["old"])).unwrap();
        let second = add_entry(&first, "fetch", &entry(&["new"])).unwrap();
        assert_eq!(second.matches("[mcp.fetch]").count(), 1, "{second}");
        let config: DevboxConfig = toml::from_str(&second).unwrap();
        assert_eq!(config.mcp["fetch"].command, ["new"]);
        assert!(second.contains("# the comment that keeps getting deleted"));
    }

    #[test]
    fn removing_one_entry_leaves_the_others_and_the_comments() {
        let text = add_entry(COMMENTED, "fetch", &entry(&["uvx", "mcp-server-fetch"])).unwrap();
        let text = add_entry(&text, "git", &entry(&["mcp-git"])).unwrap();
        let out = remove_entry(&text, "fetch")
            .unwrap()
            .expect("fetch was registered");

        assert!(!out.contains("[mcp.fetch]"), "{out}");
        assert!(out.contains("[mcp.git]"), "{out}");
        assert!(out.contains("# the comment that keeps getting deleted"));
        let config: DevboxConfig = toml::from_str(&out).unwrap();
        assert!(!config.mcp.contains_key("fetch"));
        assert_eq!(config.mcp["git"].command, ["mcp-git"]);
        assert_eq!(config.policy.allow, ["pypi.org"]);
    }

    /// Removing the *first* of two adjacent tables must not eat the second.
    #[test]
    fn removal_stops_at_the_next_table_header() {
        let text = "\
[mcp.a]
command = [\"a\"]

[mcp.b]
command = [\"b\"]

[policy]
egress = \"isolated\"
";
        let out = remove_entry(text, "a").unwrap().unwrap();
        let config: DevboxConfig = toml::from_str(&out).unwrap();
        assert!(!config.mcp.contains_key("a"));
        assert_eq!(config.mcp["b"].command, ["b"]);
        assert_eq!(config.policy.egress, Posture::Isolated);
    }

    #[test]
    fn removing_something_that_is_not_registered_says_so_instead_of_editing() {
        assert!(remove_entry(COMMENTED, "absent").unwrap().is_none());
    }

    /// A dotted or inline registration parses, so `run` and `ls` see it, but
    /// there is no block to cut — and rewriting the file would cost the user
    /// their comments. Say so rather than doing it.
    #[test]
    fn a_dotted_registration_is_refused_rather_than_rewritten() {
        let text = "# keep me\nmcp.inline = { command = [\"x\"] }\n";
        let config: DevboxConfig = toml::from_str(text).unwrap();
        assert!(
            config.mcp.contains_key("inline"),
            "the reader still sees it"
        );
        let error = remove_entry(text, "inline").unwrap_err().to_string();
        assert!(error.contains("by hand"), "{error}");
    }

    #[test]
    fn a_multiline_string_that_looks_like_a_header_does_not_end_the_block() {
        let text = "\
[mcp.a]
command = [\"a\"]

[env]
NOTE = '''
[mcp.a]
not a table
'''

[policy]
egress = \"open\"
";
        let out = remove_entry(text, "a").unwrap().unwrap();
        let config: DevboxConfig = toml::from_str(&out).unwrap();
        assert!(!config.mcp.contains_key("a"));
        assert!(
            out.contains("not a table"),
            "the string literal survived:\n{out}"
        );
    }

    #[test]
    fn names_that_would_need_quoting_are_refused() {
        for bad in ["", "a b", "a.b", "a\"b", "a/b", "../etc"] {
            assert!(validate_name(bad).is_err(), "accepted {bad:?}");
        }
        for good in ["fetch", "mcp-git", "srv_2"] {
            validate_name(good).unwrap();
        }
    }

    #[test]
    fn an_unparseable_file_is_never_edited() {
        let error = add_entry("[[[not toml", "x", &entry(&["x"])).unwrap_err();
        assert!(error.to_string().contains("does not parse"), "{error}");
    }
}
