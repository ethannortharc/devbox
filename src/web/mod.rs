//! Web console — the v4 primary interface.
//!
//! An [`axum`] server bound to loopback renders server-side HTML with
//! [`askama`], makes it live with htmx + Server-Sent Events, and shares its
//! control-plane logic with the CLI through [`service`]. All assets are
//! embedded in the binary (see [`assets`]), so the console needs no network
//! and no build step.

pub mod activity;
pub mod assets;
pub mod auth;
pub mod build;
pub mod help;
pub mod labs;
pub mod routes;
pub mod server;
pub mod service;
pub mod sse;
pub mod state;
pub mod tail;
pub mod term;
pub mod watch;

pub use server::{WebOptions, serve};

/// Percent-encode one path segment of a URL.
///
/// A box name is a directory name, and `is_safe_name` admits characters that
/// mean something else in a URL. `#` ends the path and starts a fragment, so
/// `/boxes/my#box?t=TOKEN` asks the server for `/boxes/my` and keeps the token
/// in the browser — the console gets the wrong box and no credential. `?` does
/// the same with the query, and a space is simply invalid.
///
/// Encoding rather than tightening `is_safe_name`: the name is a directory
/// name and a state-file key, so narrowing what is legal would orphan boxes
/// that already exist. This makes the URL correct for names that are already
/// out there.
///
/// Unreserved characters per RFC 3986, everything else escaped — `/` included,
/// because this is one segment.
pub fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod url_tests {
    use super::encode_segment;

    #[test]
    fn a_name_cannot_escape_its_path_segment() {
        // `#` is the one that cost a credential: the browser keeps everything
        // after it, so the launch token never reached the server and the
        // request was for a different box entirely.
        assert_eq!(encode_segment("my#box"), "my%23box");
        assert_eq!(encode_segment("a?b"), "a%3Fb");
        assert_eq!(encode_segment("with space"), "with%20space");
        assert_eq!(encode_segment("a/b"), "a%2Fb", "one segment, not two");
        // Ordinary names are untouched, so existing links do not change.
        assert_eq!(encode_segment("myapp-2.0_beta~1"), "myapp-2.0_beta~1");
    }
}
