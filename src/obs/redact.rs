//! Removing credentials from an argv, on the way out.
//!
//! Every runtime's exec takes an argv and no environment, so devbox puts the
//! broker's variables on the *command line* (`broker::with_env` → `env -- K=V
//! … cmd`). That is the only form that works uniformly across Lima, Incus and
//! Docker, and it means a box's own credentials appear in the `exec` events
//! the agent captures — and therefore in the run report's process tree, in
//! `report.json` and the JSON embedded in `report.html`, in `devbox watch
//! --tree`, and in every export.
//!
//! Nothing here is a containment boundary. The box can read `/proc/*/cmdline`
//! and see the same bytes; the broker's token is per box and rotates on
//! restart. What this protects is the *receipt* — v5's claim is that a run
//! report can be handed to someone else, and a report carrying a live token
//! cannot be.
//!
//! Two implementations, deliberately. The agent redacts at the source
//! (`agent/capture/redact.go`) so nothing is ever written down; this one
//! redacts on the way out, because every event already in a store was written
//! before that code existed, and because an agent one release behind is the
//! ordinary state of a box between upgrades. They are held to the same shapes
//! by tests on both sides.

/// What replaces a secret's value.
///
/// The same three characters the Go side writes, so someone grepping one
/// output for them finds them in the others.
pub const REDACTED: &str = "***";

/// A variable whose value is a credential.
///
/// Suffixes rather than an allowlist: the set of things ending in `_TOKEN` is
/// open, and the variable devbox has never heard of is exactly the one nobody
/// will remember to add. A false positive costs a reader the value of
/// `SORT_KEY`; a false negative costs them a live credential in a file they
/// forwarded to someone.
const SENSITIVE_ENV_SUFFIXES: [&str; 4] = ["_TOKEN", "_SECRET", "_KEY", "_CREDENTIALS"];

/// Checked anywhere in the name: `PGPASSWORD` has no separator at all.
const SENSITIVE_ENV_CONTAINS: [&str; 1] = ["PASSWORD"];

/// The bare names the suffix rule cannot reach.
///
/// `*_TOKEN` does not match `TOKEN`, and `TOKEN=abc` is not a plausible sort
/// key — it is a credential with nothing in front of it. `KEY` is deliberately
/// absent: alone it is far more often a map key or a sort field.
const SENSITIVE_ENV_EXACT: [&str; 4] = ["TOKEN", "SECRET", "PASSWD", "CREDENTIALS"];

/// A header whose value authenticates.
const SENSITIVE_HEADER_SUFFIXES: [&str; 4] = ["-token", "-key", "-secret", "-password"];

/// The exact names that carry no such suffix.
const SENSITIVE_HEADERS: [&str; 4] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
];

/// Whether a variable's value should be redacted.
pub fn is_sensitive_env(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let upper = name.to_ascii_uppercase();
    SENSITIVE_ENV_EXACT.iter().any(|exact| upper == *exact)
        || SENSITIVE_ENV_CONTAINS
            .iter()
            .any(|needle| upper.contains(needle))
        || SENSITIVE_ENV_SUFFIXES
            .iter()
            .any(|suffix| upper.ends_with(suffix))
}

/// Whether a header's value should be redacted.
pub fn is_sensitive_header(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    SENSITIVE_HEADERS.iter().any(|exact| lower == *exact)
        || SENSITIVE_HEADER_SUFFIXES
            .iter()
            .any(|suffix| lower.ends_with(suffix))
}

/// Redact every word of an argv.
pub fn argv(words: &[String]) -> Vec<String> {
    words.iter().map(|w| word(w)).collect()
}

/// Redact one argv word, returning it unchanged when there is nothing to do.
///
/// Scans *inside* the word rather than treating it as one `NAME=value`. A
/// runtime's outermost login shell carries the whole command as a single
/// quoted argument —
///
/// ```text
/// bash -c "cd /home; exec /bin/bash -l -c 'env -- DEVBOX_BROKER_TOKEN=… cmd'"
/// ```
///
/// — so a word-level split sees one enormous word with no `=` at its head, and
/// that is exactly the shape that put a live token into a run report.
pub fn word(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    redact_headers(&redact_assignments(value))
}

/// Whether a word carries anything this module would remove.
///
/// For callers that want to count rather than rewrite.
pub fn has_secret(value: &str) -> bool {
    word(value) != value
}

fn redact_assignments(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        let Some(offset) = bytes[i..].iter().position(|b| *b == b'=') else {
            out.push_str(&value[i..]);
            break;
        };
        let equals = i + offset;

        // Walk left over the name. Stopping at the first byte that cannot be
        // in one is what makes `--env=FOO` resolve to `env` and `x TOKEN=y`
        // resolve to `TOKEN` rather than to the space before it.
        let mut start = equals;
        while start > i && is_env_name_byte(bytes[start - 1]) {
            start -= 1;
        }

        if !is_sensitive_env(&value[start..equals]) {
            out.push_str(&value[i..=equals]);
            i = equals + 1;
            continue;
        }

        out.push_str(&value[i..equals]);
        out.push('=');
        out.push_str(REDACTED);
        i = equals + 1 + value_len(&bytes[equals + 1..]);
    }
    out
}

fn redact_headers(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        let Some(offset) = bytes[i..].iter().position(|b| *b == b':') else {
            out.push_str(&value[i..]);
            break;
        };
        let colon = i + offset;

        let mut start = colon;
        while start > i && is_header_name_byte(bytes[start - 1]) {
            start -= 1;
        }

        if !is_sensitive_header(&value[start..colon]) {
            out.push_str(&value[i..=colon]);
            i = colon + 1;
            continue;
        }

        out.push_str(&value[i..colon]);
        out.push_str(": ");
        out.push_str(REDACTED);
        // A header value may contain spaces (`Bearer abc`), so it runs to the
        // end of the word or to the quote that closes it — unlike an
        // assignment, which a space terminates.
        i = colon + 1 + header_value_len(&bytes[colon + 1..]);
    }
    out
}

/// How much of `rest` belongs to an assignment's value.
///
/// Whitespace ends it, and so does a quote: inside the one-word form the value
/// is followed by the next assignment or by the quote that closes the shell
/// string it lives in, and swallowing that quote would corrupt the rendering
/// of everything after it.
fn value_len(rest: &[u8]) -> usize {
    rest.iter()
        .position(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\'' | b'"'))
        .unwrap_or(rest.len())
}

fn header_value_len(rest: &[u8]) -> usize {
    rest.iter()
        .position(|b| matches!(b, b'\n' | b'\r' | b'\'' | b'"'))
        .unwrap_or(rest.len())
}

fn is_env_name_byte(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

fn is_header_name_byte(c: u8) -> bool {
    c == b'-' || c == b'_' || c.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same table `agent/capture/redact_test.go` runs. Both sides read
    /// every argv that reaches a user, and a rule enforced in one of them is
    /// a rule that stops being enforced the moment the other is the one doing
    /// the reading.
    const SHAPES: [(&str, &str); 15] = [
        ("DEVBOX_BROKER_TOKEN=sk-abc123", "DEVBOX_BROKER_TOKEN=***"),
        (
            "cd /home || exit 1 ; exec /bin/bash -l -c 'env -- DEVBOX_BROKER_TOKEN=sk-abc123 DEVBOX_RUN_ID=01X sh -c true'",
            "cd /home || exit 1 ; exec /bin/bash -l -c 'env -- DEVBOX_BROKER_TOKEN=*** DEVBOX_RUN_ID=01X sh -c true'",
        ),
        (
            "env -- ANTHROPIC_AUTH_TOKEN=a OPENAI_API_KEY=b DEVBOX_BROKER_URL=http://h:9 cmd",
            "env -- ANTHROPIC_AUTH_TOKEN=*** OPENAI_API_KEY=*** DEVBOX_BROKER_URL=http://h:9 cmd",
        ),
        (
            "A_TOKEN=1 B_SECRET=2 C_KEY=3 D_CREDENTIALS=4 PGPASSWORD=5 lower_token=6",
            "A_TOKEN=*** B_SECRET=*** C_KEY=*** D_CREDENTIALS=*** PGPASSWORD=*** lower_token=***",
        ),
        (
            "TOKEN=1 SECRET=2 CREDENTIALS=3",
            "TOKEN=*** SECRET=*** CREDENTIALS=***",
        ),
        // `KEY` alone is deliberately not sensitive: far more often a map key
        // or a sort field than a credential.
        ("sort --key=3 KEY=name", "sort --key=3 KEY=name"),
        (
            "DEVBOX_RUN_ID=01M1SCJ6KHE90VMWKN04C5QQVT",
            "DEVBOX_RUN_ID=01M1SCJ6KHE90VMWKN04C5QQVT",
        ),
        (
            "DEVBOX_BROKER_URL=http://host.lima.internal:7879",
            "DEVBOX_BROKER_URL=http://host.lima.internal:7879",
        ),
        ("Authorization: Bearer sk-abc123", "Authorization: ***"),
        (
            "curl -s -H 'Authorization: Bearer sk-abc123' https://api",
            "curl -s -H 'Authorization: ***' https://api",
        ),
        (
            "-H 'x-devbox-broker-token: abc123'",
            "-H 'x-devbox-broker-token: ***'",
        ),
        ("-H 'x-api-key: abc123'", "-H 'x-api-key: ***'"),
        (
            "curl https://example.com:443/x PATH=/usr/bin:/bin at 10:00:00",
            "curl https://example.com:443/x PATH=/usr/bin:/bin at 10:00:00",
        ),
        ("", ""),
        ("=value", "=value"),
    ];

    #[test]
    fn every_shape_an_argv_produces_is_covered() {
        for (input, expected) in SHAPES {
            assert_eq!(word(input), expected, "redacting {input:?}");
        }
    }

    #[test]
    fn redaction_is_idempotent() {
        // The agent redacts at the source and this redacts on the way out, so
        // an already-clean event goes through the rule a second time. A pass
        // that corrupted its own output would show up as a mangled command
        // line in every report from an up-to-date box.
        for (input, expected) in SHAPES {
            assert_eq!(word(&word(input)), expected, "second pass over {input:?}");
        }
    }

    #[test]
    fn a_whole_argv_is_redacted_word_by_word() {
        let original: Vec<String> = ["sh", "-c", "DEVBOX_BROKER_TOKEN=abc123 true"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let clean = argv(&original);
        assert_eq!(clean[0], "sh");
        assert_eq!(clean[2], "DEVBOX_BROKER_TOKEN=*** true");
        assert!(has_secret(&original[2]));
        assert!(!has_secret(&original[0]));
    }

    #[test]
    fn a_multibyte_command_line_is_not_split_through_a_character() {
        // A command line is guest-influenced text. Indexing it by byte is
        // correct only if every index lands on a boundary, and the scanner
        // walks backwards from an `=` it found by byte.
        let value = "echo 日本語 API_TOKEN=秘密 のこり";
        assert_eq!(word(value), "echo 日本語 API_TOKEN=*** のこり");
        assert_eq!(
            word("日本語=x"),
            "日本語=x",
            "a non-ASCII name is not a match"
        );
    }
}
