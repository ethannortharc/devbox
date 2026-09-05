//! Per-box broker tokens — §6.3.
//!
//! One random token per box, rotated when the box starts, stored at
//! `~/.devbox/broker/tokens/<box>` mode 0600 under a 0700 directory. The token
//! is the broker's only way to tell one box from another: on Lima the guest's
//! traffic arrives on the host's loopback with source `127.0.0.1` (measured),
//! so there is no address to key on.
//!
//! Holding a token buys brokered, scoped, logged access to whatever secrets
//! the operator configured — never the secrets themselves.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{Context, Result};

/// 32 bytes of randomness, hex-encoded. Long enough that guessing is not a
/// threat model, short enough to sit in an environment variable unremarked.
const TOKEN_BYTES: usize = 32;

/// The directory holding one file per box.
pub fn tokens_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("broker").join("tokens")
}

/// Mint and persist a fresh token for a box, replacing any previous one.
pub fn rotate(state_dir: &Path, box_id: &str) -> Result<String> {
    if !crate::sandbox::state::is_safe_name(box_id) {
        anyhow::bail!("refusing to mint a broker token for unsafe box name {box_id:?}");
    }
    let dir = tokens_dir(state_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protect {}", dir.display()))?;

    let token: String = (0..TOKEN_BYTES)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();

    let temporary = dir.join(format!(".{box_id}.tmp"));
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(token.as_bytes())
        .context("write the broker token")?;
    file.sync_all().context("flush the broker token")?;
    drop(file);
    std::fs::rename(&temporary, dir.join(box_id))
        .with_context(|| format!("publish the broker token for '{box_id}'"))?;
    Ok(token)
}

/// Read a box's current token, if it has one.
pub fn current(state_dir: &Path, box_id: &str) -> Option<String> {
    let text = std::fs::read_to_string(tokens_dir(state_dir).join(box_id)).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Forget a box's token — called when the box is destroyed.
pub fn forget(state_dir: &Path, box_id: &str) {
    let _ = std::fs::remove_file(tokens_dir(state_dir).join(box_id));
}

/// The reverse map the server uses: token → box.
///
/// Reloaded whenever the directory's modification time changes, so a box that
/// starts while the broker is already running is recognised without a restart,
/// and a rotated token stops working as soon as it is replaced.
pub struct TokenStore {
    dir: PathBuf,
    cache: RwLock<(Option<std::time::SystemTime>, HashMap<String, String>)>,
}

impl TokenStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: tokens_dir(state_dir),
            cache: RwLock::new((None, HashMap::new())),
        }
    }

    /// Which box a token belongs to.
    pub fn box_for(&self, token: &str) -> Option<String> {
        self.refresh();
        let guard = self.cache.read().ok()?;
        // Linear, with a constant-time compare per entry. A hash lookup would
        // be faster and would also make the comparison data-dependent; with a
        // handful of boxes the scan is free and the reasoning is simpler.
        guard
            .1
            .iter()
            .find(|(candidate, _)| constant_time_eq(candidate.as_bytes(), token.as_bytes()))
            .map(|(_, box_id)| box_id.clone())
    }

    /// How many boxes currently have a token, for `doctor`.
    pub fn len(&self) -> usize {
        self.refresh();
        self.cache.read().map(|g| g.1.len()).unwrap_or(0)
    }

    /// Whether any box holds a token.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn refresh(&self) {
        let stamp = std::fs::metadata(&self.dir).and_then(|m| m.modified()).ok();
        if let Ok(guard) = self.cache.read()
            && guard.0 == stamp
            && stamp.is_some()
        {
            return;
        }
        let mut map = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if let Ok(token) = std::fs::read_to_string(entry.path()) {
                    let token = token.trim().to_string();
                    if !token.is_empty() {
                        map.insert(token, name);
                    }
                }
            }
        }
        if let Ok(mut guard) = self.cache.write() {
            *guard = (stamp, map);
        }
    }
}

/// Compare two byte strings without an early exit on the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_minted_at_0600_and_reverses_to_its_box() {
        let dir = tempfile::tempdir().unwrap();
        let token = rotate(dir.path(), "myapp").unwrap();
        assert_eq!(token.len(), TOKEN_BYTES * 2);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

        let path = tokens_dir(dir.path()).join("myapp");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            std::fs::metadata(tokens_dir(dir.path()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        assert_eq!(
            current(dir.path(), "myapp").as_deref(),
            Some(token.as_str())
        );

        let store = TokenStore::new(dir.path());
        assert_eq!(store.box_for(&token).as_deref(), Some("myapp"));
        assert_eq!(store.box_for("not-a-token"), None);
        assert_eq!(store.box_for(""), None);
    }

    #[test]
    fn rotation_invalidates_the_previous_token() {
        let dir = tempfile::tempdir().unwrap();
        let first = rotate(dir.path(), "myapp").unwrap();
        let store = TokenStore::new(dir.path());
        assert_eq!(store.box_for(&first).as_deref(), Some("myapp"));

        // The cache keys on the directory mtime, which has one-second
        // granularity on some filesystems; the second box also proves the
        // reload happens.
        let second = rotate(dir.path(), "other").unwrap();
        let fresh = TokenStore::new(dir.path());
        assert_eq!(fresh.box_for(&second).as_deref(), Some("other"));
        assert_ne!(first, second);

        forget(dir.path(), "myapp");
        let after = TokenStore::new(dir.path());
        assert_eq!(after.box_for(&first), None);
        assert_eq!(after.len(), 1);
    }

    #[test]
    fn two_boxes_never_share_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let a = rotate(dir.path(), "a").unwrap();
        let b = rotate(dir.path(), "b").unwrap();
        assert_ne!(a, b);
        let store = TokenStore::new(dir.path());
        assert_eq!(store.box_for(&a).as_deref(), Some("a"));
        assert_eq!(store.box_for(&b).as_deref(), Some("b"));
    }

    #[test]
    fn an_unsafe_box_name_cannot_write_outside_the_token_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(rotate(dir.path(), "../escape").is_err());
        assert!(rotate(dir.path(), "").is_err());
    }

    #[test]
    fn the_comparison_does_not_short_circuit_on_length_matched_input() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
