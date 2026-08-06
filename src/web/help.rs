//! Help view — the migrated cheat sheets.
//!
//! v3 shipped these as terminal markdown, rendered by `glow` inside a Zellij
//! floating pane. v4 keeps the same source files (`help/*.md`, still embedded
//! and still served by `devbox guide`) and renders them as HTML in the console,
//! so retiring the TUI loses no content — §5 of the design.

use pulldown_cmark::{Options, Parser, html};
use serde::Serialize;

use crate::cli::help::CHEAT_SHEETS;

/// A cheat sheet as the Help view lists it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Topic {
    /// URL slug and lookup key, e.g. `lazygit`.
    pub slug: String,
    /// Display name — the sheet's first `# ` heading, falling back to the slug.
    pub title: String,
}

/// Every topic except the index, which is the Help landing page itself.
pub fn topics() -> Vec<Topic> {
    CHEAT_SHEETS
        .iter()
        .filter(|(slug, _)| *slug != "index")
        .map(|(slug, body)| Topic {
            slug: (*slug).to_string(),
            title: title_of(body).unwrap_or_else(|| (*slug).to_string()),
        })
        .collect()
}

/// Pull the first level-1 heading out of a markdown document.
pub fn title_of(markdown: &str) -> Option<String> {
    markdown.lines().find_map(|line| {
        line.strip_prefix("# ")
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    })
}

/// Look up a cheat sheet's markdown source by slug.
pub fn source(slug: &str) -> Option<&'static str> {
    CHEAT_SHEETS
        .iter()
        .find(|(name, _)| *name == slug)
        .map(|(_, body)| *body)
}

/// Render markdown to HTML.
///
/// The input is compiled into the binary, never user-supplied, so the escaping
/// question is about correctness rather than safety: tables and strikethrough
/// are enabled because the sheets use them, and raw HTML passes through
/// because a couple of sheets rely on it.
pub fn to_html(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_FOOTNOTES);

    let mut out = String::with_capacity(markdown.len() * 3 / 2);
    html::push_html(&mut out, Parser::new_ext(markdown, options));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cheat_sheet_except_the_index_is_a_topic() {
        let topics = topics();
        assert_eq!(topics.len(), CHEAT_SHEETS.len() - 1);
        assert!(topics.iter().all(|t| t.slug != "index"));
        assert!(topics.iter().any(|t| t.slug == "lazygit"));
    }

    #[test]
    fn every_topic_has_a_non_empty_title() {
        for t in topics() {
            assert!(!t.title.is_empty(), "topic {} has no title", t.slug);
        }
    }

    #[test]
    fn title_comes_from_the_first_h1() {
        assert_eq!(title_of("# Git\n\nstuff"), Some("Git".to_string()));
        assert_eq!(
            title_of("intro\n\n# Real Title\n"),
            Some("Real Title".into())
        );
        // A level-2 heading is not a title.
        assert_eq!(title_of("## Not a title"), None);
        assert_eq!(title_of("no headings here"), None);
        assert_eq!(title_of("# \n"), None);
    }

    #[test]
    fn source_lookup() {
        assert!(source("git").is_some());
        assert!(source("index").is_some());
        assert!(source("nope").is_none());
    }

    #[test]
    fn markdown_renders_to_html() {
        let html = to_html("# Title\n\nSome `code` and a [link](https://x).\n");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("href=\"https://x\""));
    }

    #[test]
    fn tables_render_because_the_sheets_use_them() {
        let html = to_html("| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(html.contains("<table>"), "got: {html}");
        assert!(html.contains("<td>1</td>"));
    }

    #[test]
    fn all_embedded_sheets_render_without_panicking() {
        for (slug, body) in CHEAT_SHEETS {
            let html = to_html(body);
            assert!(!html.is_empty(), "{slug} rendered empty");
        }
    }
}
