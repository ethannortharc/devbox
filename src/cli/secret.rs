//! `devbox secret` — the credentials the broker holds on the box's behalf (§6.2).
//!
//! Four verbs and one rule: nothing here ever prints a secret value. `ls`
//! shows names and backends, `set` reads the value from a place that keeps it
//! off the command line, `rm` removes it, and `scope` narrows what a box may
//! do with it. There is deliberately no `get`.

use std::io::Read as _;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::broker::providers::{HttpProvider, Injection};
use crate::broker::scope::{Access, Scope};
use crate::broker::secrets::{Backend, SecretStore, check_name};
use crate::sandbox::SandboxManager;

#[derive(Args, Debug)]
pub struct SecretArgs {
    #[command(subcommand)]
    pub command: SecretCommand,
}

#[derive(Subcommand, Debug)]
pub enum SecretCommand {
    /// Store a credential on the host, for the broker to inject
    Set(SetArgs),

    /// List configured providers — names and backends only, never values
    #[command(alias = "list")]
    Ls,

    /// Remove a stored credential
    #[command(alias = "remove")]
    Rm(RmArgs),

    /// Show or change what a box may do with a provider's credential
    Scope(ScopeArgs),
}

#[derive(Args, Debug)]
pub struct SetArgs {
    /// Provider: anthropic, openai, github, or a name of your own
    #[arg(value_name = "PROVIDER")]
    pub provider: String,

    /// Read the value from this environment variable
    #[arg(long, value_name = "VAR", conflicts_with_all = ["stdin", "from_keychain"])]
    pub from_env: Option<String>,

    /// Read the value from the macOS Keychain: SERVICE ACCOUNT
    #[arg(long, num_args = 2, value_names = ["SERVICE", "ACCOUNT"], conflicts_with = "stdin")]
    pub from_keychain: Option<Vec<String>>,

    /// Read the value from standard input
    #[arg(long)]
    pub stdin: bool,

    /// Upstream base URL, for a provider that is not built in
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,

    /// Header to inject, e.g. 'Authorization: Bearer' or 'X-Api-Key'
    #[arg(long, value_name = "SPEC")]
    pub header: Option<String>,
}

#[derive(Args, Debug)]
pub struct RmArgs {
    #[arg(value_name = "PROVIDER")]
    pub provider: String,
}

#[derive(Args, Debug)]
pub struct ScopeArgs {
    #[arg(value_name = "PROVIDER")]
    pub provider: String,

    /// Add a repository to the allowlist, as owner/name (github only)
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Vec<String>,

    /// Access level: read, push, or admin
    #[arg(long, value_name = "LEVEL")]
    pub allow: Option<String>,

    /// Restrict to these upstream path prefixes
    #[arg(long = "path", value_name = "PREFIX")]
    pub paths: Vec<String>,

    /// Drop the repository allowlist, falling back to the project's remotes
    #[arg(long)]
    pub clear_repos: bool,
}

pub async fn run(args: SecretArgs, manager: &SandboxManager) -> Result<()> {
    match args.command {
        SecretCommand::Set(args) => set(args, manager),
        SecretCommand::Ls => list(manager),
        SecretCommand::Rm(args) => remove(args, manager),
        SecretCommand::Scope(args) => scope(args, manager),
    }
}

fn set(args: SetArgs, manager: &SandboxManager) -> Result<()> {
    check_name(&args.provider)?;

    // A generic provider needs an upstream and a header before its secret is
    // worth anything; refusing here beats a 404 from the broker later.
    let builtin = crate::broker::providers::is_builtin(&args.provider);
    if !builtin {
        let existing = crate::broker::http_provider_path(&manager.state_dir, &args.provider)
            .filter(|path| path.exists())
            .is_some();
        if args.url.is_none() && !existing {
            bail!(
                "'{}' is not a built-in provider, so it needs an upstream: \
                 pass --url and --header",
                args.provider
            );
        }
    } else if args.url.is_some() {
        bail!(
            "'{}' is a built-in provider; its upstream is fixed and --url would be ignored",
            args.provider
        );
    }

    let value = read_value(&args)?;
    let store = SecretStore::open(&manager.state_dir);
    store.set(&args.provider, &value)?;
    crate::broker::index_add(&manager.state_dir, &args.provider)?;
    // The value is not needed again. Rust will drop it, but zeroing what we
    // can reach is cheap and makes the intent unmistakable to the next reader.
    drop(value);

    if let Some(url) = &args.url {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            bail!("--url must start with http:// or https://");
        }
        let injection = match &args.header {
            Some(spec) => Injection::parse(spec)?,
            None => Injection::bearer(),
        };
        let path = crate::broker::http_provider_path(&manager.state_dir, &args.provider)
            .context("invalid provider name")?;
        crate::broker::write_json(
            &path,
            &HttpProvider {
                url: url.clone(),
                injection,
            },
        )?;
    }

    println!(
        "Stored '{}' in the {} store. It is never written into a box.",
        args.provider,
        store.backend().as_str()
    );
    println!("Restart a box, or run `devbox broker start`, to pick it up.");
    Ok(())
}

/// Read the value without ever putting it on a command line.
///
/// There is no `--value` flag on purpose: an argv is visible in `ps` to every
/// process on the host and lands in the shell history of the person typing it.
fn read_value(args: &SetArgs) -> Result<String> {
    if let Some(var) = &args.from_env {
        let value =
            std::env::var(var).with_context(|| format!("environment variable {var} is not set"))?;
        if value.trim().is_empty() {
            bail!("environment variable {var} is empty");
        }
        return Ok(value.trim().to_string());
    }
    if let Some(pair) = &args.from_keychain {
        let [service, account] = pair.as_slice() else {
            bail!("--from-keychain takes SERVICE and ACCOUNT");
        };
        let out = std::process::Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .stdin(std::process::Stdio::null())
            .output()
            .context("run security(1)")?;
        if !out.status.success() {
            bail!("no keychain item for service '{service}', account '{account}'");
        }
        let value = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
        if value.is_empty() {
            bail!("the keychain item is empty");
        }
        return Ok(value);
    }
    if args.stdin {
        let mut value = String::new();
        std::io::stdin()
            .read_to_string(&mut value)
            .context("read the secret from stdin")?;
        let value = value.trim().to_string();
        if value.is_empty() {
            bail!("nothing arrived on stdin");
        }
        return Ok(value);
    }
    bail!("choose one of --from-env VAR, --from-keychain SERVICE ACCOUNT, or --stdin")
}

fn list(manager: &SandboxManager) -> Result<()> {
    let store = SecretStore::open(&manager.state_dir);
    let configured = crate::broker::configured_providers(&manager.state_dir);
    if configured.is_empty() {
        println!("No secrets are configured.");
        println!("  devbox secret set anthropic --from-env ANTHROPIC_API_KEY");
        return Ok(());
    }
    let http: std::collections::HashMap<_, _> = crate::broker::http_providers(&manager.state_dir)
        .into_iter()
        .collect();

    println!("{:<20} {:<12} UPSTREAM", "PROVIDER", "STORE");
    println!("{}", "-".repeat(72));
    for provider in &configured {
        // The index says a secret was stored; the store says whether it is
        // still there. They can disagree if a keychain item was deleted by
        // hand, and silently listing a provider that no longer works is how
        // that turns into a mystery 403.
        let present = store.has(provider);
        let upstream = match provider.as_str() {
            "anthropic" => "https://api.anthropic.com".to_string(),
            "openai" => "https://api.openai.com".to_string(),
            "github" => "https://api.github.com, https://github.com".to_string(),
            other => http
                .get(other)
                .map(|p| p.url.clone())
                .unwrap_or_else(|| "(no upstream configured)".to_string()),
        };
        println!(
            "{:<20} {:<12} {}{}",
            provider,
            store.backend().as_str(),
            upstream,
            if present { "" } else { "  (value missing!)" }
        );
    }
    println!();
    println!("Values are never printed, and never enter a box.");
    Ok(())
}

fn remove(args: RmArgs, manager: &SandboxManager) -> Result<()> {
    check_name(&args.provider)?;
    let store = SecretStore::open(&manager.state_dir);
    let removed = store.remove(&args.provider)?;
    crate::broker::index_remove(&manager.state_dir, &args.provider);
    if let Some(path) = crate::broker::http_provider_path(&manager.state_dir, &args.provider) {
        let _ = std::fs::remove_file(path);
    }
    if let Some(path) = crate::broker::scope_path(&manager.state_dir, &args.provider) {
        let _ = std::fs::remove_file(path);
    }
    if removed {
        println!("Removed '{}'.", args.provider);
    } else {
        println!("No secret was stored for '{}'.", args.provider);
    }
    Ok(())
}

fn scope(args: ScopeArgs, manager: &SandboxManager) -> Result<()> {
    check_name(&args.provider)?;
    let path = crate::broker::scope_path(&manager.state_dir, &args.provider)
        .context("invalid provider name")?;
    let mut scope: Scope = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();

    let changing =
        !args.repo.is_empty() || args.allow.is_some() || !args.paths.is_empty() || args.clear_repos;

    if let Some(level) = &args.allow {
        scope.access = level.parse::<Access>()?;
    }
    if !args.paths.is_empty() {
        scope.paths = args.paths.clone();
    }
    if args.clear_repos {
        scope.repos = None;
    }
    for repo in &args.repo {
        let (owner, name) = repo
            .split_once('/')
            .with_context(|| format!("--repo takes owner/name, got '{repo}'"))?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            bail!("--repo takes owner/name, got '{repo}'");
        }
        scope
            .repos
            .get_or_insert_with(Default::default)
            .insert(repo.to_ascii_lowercase());
    }

    if changing {
        crate::broker::write_json(&path, &scope)?;
    }

    println!("scope for '{}':", args.provider);
    println!("  methods: {}", scope.access.methods().join(", "));
    println!(
        "  paths:   {}",
        if scope.paths.is_empty() {
            "(any)".to_string()
        } else {
            scope.paths.join(", ")
        }
    );
    if args.provider == crate::broker::providers::GITHUB {
        match &scope.repos {
            Some(repos) if repos.is_empty() => {
                println!("  repos:   (none — every request is denied)")
            }
            Some(repos) => println!(
                "  repos:   {}",
                repos.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
            None => {
                let cwd = std::env::current_dir().unwrap_or_default();
                let detected = crate::broker::scope::project_repos(&cwd);
                if detected.is_empty() {
                    println!(
                        "  repos:   (default: this project's github remotes — none found here)"
                    );
                } else {
                    println!(
                        "  repos:   (default: this project's github remotes) {}",
                        detected.iter().cloned().collect::<Vec<_>>().join(", ")
                    );
                }
            }
        }
    }
    Ok(())
}

/// Whether this host has a Keychain-backed store, for `doctor`.
pub fn backend_label(manager: &SandboxManager) -> &'static str {
    match SecretStore::open(&manager.state_dir).backend() {
        Backend::Keychain => "macOS Keychain",
        Backend::SecretTool => "Secret Service (secret-tool)",
        Backend::File => "~/.devbox/secrets (0600)",
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::cli::{Cli, Command};

    #[test]
    fn there_is_no_command_that_prints_a_secret() {
        use clap::CommandFactory as _;
        let root = Cli::command();
        let secret = root
            .get_subcommands()
            .find(|c| c.get_name() == "secret")
            .expect("devbox secret exists");
        let verbs: Vec<&str> = secret.get_subcommands().map(|c| c.get_name()).collect();
        assert!(verbs.contains(&"set"));
        assert!(verbs.contains(&"ls"));
        assert!(verbs.contains(&"rm"));
        assert!(verbs.contains(&"scope"));
        for forbidden in ["get", "show", "cat", "print", "export"] {
            assert!(
                !verbs.contains(&forbidden),
                "`devbox secret {forbidden}` would defeat the point of the component"
            );
        }
    }

    #[test]
    fn a_secret_value_cannot_be_passed_on_the_command_line() {
        use clap::CommandFactory as _;
        let root = Cli::command();
        let set = root
            .get_subcommands()
            .find(|c| c.get_name() == "secret")
            .unwrap()
            .get_subcommands()
            .find(|c| c.get_name() == "set")
            .unwrap()
            .clone();
        for arg in set.get_arguments() {
            let id = arg.get_id().as_str();
            assert!(
                !matches!(id, "value" | "secret" | "key" | "token"),
                "`--{id}` would put the credential in every `ps` on this host"
            );
        }
    }

    #[test]
    fn the_provider_is_a_positional_like_every_other_v5_name() {
        let cli = Cli::try_parse_from(["devbox", "secret", "rm", "anthropic"]).unwrap();
        let Some(Command::Secret(args)) = cli.command else {
            panic!("not a secret command");
        };
        let super::SecretCommand::Rm(rm) = args.command else {
            panic!("not rm");
        };
        assert_eq!(rm.provider, "anthropic");
    }

    #[test]
    fn set_requires_exactly_one_source_and_they_conflict() {
        assert!(Cli::try_parse_from(["devbox", "secret", "set", "anthropic"]).is_ok());
        let error = Cli::try_parse_from([
            "devbox",
            "secret",
            "set",
            "anthropic",
            "--stdin",
            "--from-env",
            "X",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn scope_parses_repeated_repos_and_an_access_level() {
        let cli = Cli::try_parse_from([
            "devbox", "secret", "scope", "github", "--repo", "a/b", "--repo", "c/d", "--allow",
            "read",
        ])
        .unwrap();
        let Some(Command::Secret(args)) = cli.command else {
            panic!()
        };
        let super::SecretCommand::Scope(scope) = args.command else {
            panic!()
        };
        assert_eq!(scope.repo, vec!["a/b", "c/d"]);
        assert_eq!(scope.allow.as_deref(), Some("read"));
    }
}
