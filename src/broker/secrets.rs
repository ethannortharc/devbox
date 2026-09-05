//! Where the credentials live — §6.2.
//!
//! The one rule this module exists to enforce: a secret value crosses process
//! boundaries exactly twice — in, from the operator, and out, into one
//! outbound request header. It is never written to `state.json`, `devbox.toml`,
//! the event store, a log line, or the guest.
//!
//! Storage is per platform:
//!
//! - macOS: the login Keychain, through `security(1)`. Service `devbox`,
//!   account `<provider>`. The value is passed to `security` on stdin-free
//!   argv, which is the interface `security` offers; see [`Backend::Keychain`]
//!   for why that is acceptable here and what it costs.
//! - Linux: `secret-tool` when a Secret Service is running, otherwise
//!   `~/.devbox/secrets/<provider>` at mode 0600 under a 0700 directory.
//!
//! `devbox secret ls` prints names and backends. There is no command that
//! prints a value, and adding one would defeat the module.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// The Keychain service name, and the `secret-tool` attribute value.
const SERVICE: &str = "devbox";

/// Which store a secret came out of, for `devbox secret ls`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// macOS login Keychain via `security(1)`.
    Keychain,
    /// Freedesktop Secret Service via `secret-tool(1)`.
    SecretTool,
    /// `~/.devbox/secrets/<provider>`, mode 0600.
    File,
}

impl Backend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Keychain => "keychain",
            Backend::SecretTool => "secret-tool",
            Backend::File => "file",
        }
    }
}

/// A secret store rooted at a devbox state directory.
///
/// The state directory only matters for the file backend, but the store takes
/// it unconditionally so tests can pin the whole thing to a temporary
/// directory without reaching into the user's real Keychain.
#[derive(Debug, Clone)]
pub struct SecretStore {
    dir: PathBuf,
    backend: Backend,
}

impl SecretStore {
    /// The store this host would use by default.
    pub fn open(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("secrets"),
            backend: default_backend(),
        }
    }

    /// A store pinned to the file backend — used by the tests, and by anyone
    /// on a desktopless Linux host.
    pub fn file_backed(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("secrets"),
            backend: Backend::File,
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Store `value` under `provider`, replacing any previous value.
    pub fn set(&self, provider: &str, value: &str) -> Result<()> {
        check_name(provider)?;
        if value.is_empty() {
            bail!("refusing to store an empty secret for '{provider}'");
        }
        match self.backend {
            Backend::Keychain => {
                // `-U` updates in place; without it a second `set` fails with
                // "already exists" and the operator is left with the old value
                // while the command looked like it might have worked.
                let out = Command::new("security")
                    .args([
                        "add-generic-password",
                        "-U",
                        "-s",
                        SERVICE,
                        "-a",
                        provider,
                        "-w",
                        value,
                    ])
                    .stdin(Stdio::null())
                    .output()
                    .context("run security(1) to store the secret")?;
                if !out.status.success() {
                    bail!(
                        "security(1) refused to store the secret: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                Ok(())
            }
            Backend::SecretTool => {
                let mut child = Command::new("secret-tool")
                    .args([
                        "store",
                        "--label",
                        &format!("devbox {provider}"),
                        "service",
                        SERVICE,
                        "account",
                        provider,
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .spawn()
                    .context("run secret-tool to store the secret")?;
                child
                    .stdin
                    .as_mut()
                    .context("secret-tool has no stdin")?
                    .write_all(value.as_bytes())
                    .context("write the secret to secret-tool")?;
                let status = child.wait().context("wait for secret-tool")?;
                if !status.success() {
                    bail!("secret-tool refused to store the secret");
                }
                Ok(())
            }
            Backend::File => {
                std::fs::create_dir_all(&self.dir)
                    .with_context(|| format!("create {}", self.dir.display()))?;
                std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("protect {}", self.dir.display()))?;
                let path = self.dir.join(provider);
                // `create_new` on a fresh temporary and then rename: a reader
                // never observes a half-written secret, and the mode is set
                // before any bytes exist rather than after.
                let temporary = self.dir.join(format!(".{provider}.tmp"));
                let _ = std::fs::remove_file(&temporary);
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&temporary)
                    .with_context(|| format!("create {}", temporary.display()))?;
                file.write_all(value.as_bytes())
                    .context("write the secret")?;
                file.sync_all().context("flush the secret")?;
                drop(file);
                std::fs::rename(&temporary, &path)
                    .with_context(|| format!("publish {}", path.display()))?;
                Ok(())
            }
        }
    }

    /// Read a secret back, or `None` when the provider has none.
    pub fn get(&self, provider: &str) -> Result<Option<String>> {
        check_name(provider)?;
        match self.backend {
            Backend::Keychain => {
                let out = Command::new("security")
                    .args(["find-generic-password", "-s", SERVICE, "-a", provider, "-w"])
                    .stdin(Stdio::null())
                    .output()
                    .context("run security(1) to read the secret")?;
                if !out.status.success() {
                    return Ok(None);
                }
                let value = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
                Ok(if value.is_empty() { None } else { Some(value) })
            }
            Backend::SecretTool => {
                let out = Command::new("secret-tool")
                    .args(["lookup", "service", SERVICE, "account", provider])
                    .stdin(Stdio::null())
                    .output()
                    .context("run secret-tool to read the secret")?;
                if !out.status.success() {
                    return Ok(None);
                }
                let value = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
                Ok(if value.is_empty() { None } else { Some(value) })
            }
            Backend::File => {
                let path = self.dir.join(provider);
                match std::fs::read_to_string(&path) {
                    Ok(value) => {
                        let value = value.trim_end_matches(['\r', '\n']).to_string();
                        Ok(if value.is_empty() { None } else { Some(value) })
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => {
                        Err(error).with_context(|| format!("read secret {}", path.display()))
                    }
                }
            }
        }
    }

    /// Remove a secret. Returns whether one was there.
    pub fn remove(&self, provider: &str) -> Result<bool> {
        check_name(provider)?;
        match self.backend {
            Backend::Keychain => {
                let out = Command::new("security")
                    .args(["delete-generic-password", "-s", SERVICE, "-a", provider])
                    .stdin(Stdio::null())
                    .output()
                    .context("run security(1) to delete the secret")?;
                Ok(out.status.success())
            }
            Backend::SecretTool => {
                let out = Command::new("secret-tool")
                    .args(["clear", "service", SERVICE, "account", provider])
                    .stdin(Stdio::null())
                    .output()
                    .context("run secret-tool to delete the secret")?;
                Ok(out.status.success())
            }
            Backend::File => {
                let path = self.dir.join(provider);
                match std::fs::remove_file(&path) {
                    Ok(()) => Ok(true),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(error) => {
                        Err(error).with_context(|| format!("remove secret {}", path.display()))
                    }
                }
            }
        }
    }

    /// Whether a provider has a secret, without reading it.
    ///
    /// Used on every session-environment build, where the answer decides
    /// whether `ANTHROPIC_BASE_URL` is set at all. Reading the value to answer
    /// a yes/no question would put it in this process's memory for no reason —
    /// but neither the Keychain nor the Secret Service offers an existence
    /// probe, so on those backends this is `get(..).is_some()` and the comment
    /// is the honest statement of what it costs.
    pub fn has(&self, provider: &str) -> bool {
        match self.backend {
            Backend::File => self.dir.join(provider).exists(),
            _ => matches!(self.get(provider), Ok(Some(_))),
        }
    }
}

/// The backend this host would pick.
fn default_backend() -> Backend {
    if cfg!(target_os = "macos") && which::which("security").is_ok() {
        return Backend::Keychain;
    }
    if which::which("secret-tool").is_ok() && std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
    {
        return Backend::SecretTool;
    }
    Backend::File
}

/// Provider names become filenames, Keychain accounts, and URL path segments.
///
/// Rejecting anything outside this set at the door is what keeps
/// `devbox secret set ../../etc/passwd` from being interesting, and what keeps
/// a provider name from having to be escaped again at every later use.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("secret name must be 1-64 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("secret name '{name}' may contain only letters, digits, '-', '_' and '.'");
    }
    if name.starts_with('.') {
        bail!("secret name must not start with '.'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_backend_round_trips_at_0600_under_a_0700_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::file_backed(dir.path());

        assert!(store.get("anthropic").unwrap().is_none());
        assert!(!store.has("anthropic"));

        store.set("anthropic", "sk-test-value").unwrap();
        assert_eq!(
            store.get("anthropic").unwrap().as_deref(),
            Some("sk-test-value")
        );
        assert!(store.has("anthropic"));

        let secret = dir.path().join("secrets").join("anthropic");
        let mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "a secret readable by the group is not a secret"
        );
        let dir_mode = std::fs::metadata(dir.path().join("secrets"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);

        // Overwriting replaces rather than appends.
        store.set("anthropic", "second").unwrap();
        assert_eq!(store.get("anthropic").unwrap().as_deref(), Some("second"));

        assert!(store.remove("anthropic").unwrap());
        assert!(!store.remove("anthropic").unwrap());
        assert!(store.get("anthropic").unwrap().is_none());
    }

    #[test]
    fn a_trailing_newline_from_a_heredoc_is_not_part_of_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::file_backed(dir.path());
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets").join("x"), "value\n").unwrap();
        assert_eq!(store.get("x").unwrap().as_deref(), Some("value"));
    }

    #[test]
    fn names_that_would_escape_the_secrets_directory_are_refused() {
        for bad in [
            "",
            "../etc/passwd",
            "a/b",
            ".hidden",
            "with space",
            "semi;colon",
        ] {
            assert!(check_name(bad).is_err(), "{bad:?} must be refused");
        }
        for good in ["anthropic", "http-echo", "my_secret", "v1.2"] {
            check_name(good).unwrap();
        }
    }

    #[test]
    fn an_empty_value_is_refused_rather_than_stored() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::file_backed(dir.path());
        assert!(store.set("anthropic", "").is_err());
    }
}
