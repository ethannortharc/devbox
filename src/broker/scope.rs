//! What a box is allowed to do with a credential it does not hold — §6.4.
//!
//! A scope is three independent filters, all of which must pass:
//!
//! 1. **Method** — the HTTP verbs the provider may be used with.
//! 2. **Path prefix** — the upstream paths it may reach.
//! 3. **Repository** — for `github` only, the `owner/name` pairs it may touch.
//!
//! The repository filter is the one that has to be right. Its default is the
//! set of repositories the *project directory* already points at, read from
//! `git remote -v`, because that is the only default that is both useful and
//! defensible: an agent working on a checkout can push to the checkout's own
//! remote and nothing else. A project with no remotes gets an empty set, which
//! denies everything and says so — the alternative, "no default means allow
//! all", would hand a token for every repository the operator can reach to a
//! box that asked for none of them.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// The verbs a scope grants, as a named level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Read-only: `GET`, `HEAD`.
    Read,
    /// Read plus the verbs a push or a comment needs.
    Push,
    /// Everything, including `DELETE`.
    Admin,
}

impl Access {
    pub fn methods(&self) -> &'static [&'static str] {
        match self {
            Access::Read => &["GET", "HEAD"],
            Access::Push => &["GET", "HEAD", "POST", "PUT", "PATCH"],
            Access::Admin => &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"],
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Push => "push",
            Access::Admin => "admin",
        }
    }
}

impl std::str::FromStr for Access {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "read" => Ok(Access::Read),
            "push" => Ok(Access::Push),
            "admin" => Ok(Access::Admin),
            other => bail!("unknown access level '{other}' (want read, push, or admin)"),
        }
    }
}

/// One provider's scope, as persisted under `~/.devbox/broker/scopes/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub access: Access,
    /// Upstream path prefixes. Empty means every path the provider serves.
    #[serde(default)]
    pub paths: Vec<String>,
    /// `owner/name`, lowercased. Only consulted for the `github` provider.
    ///
    /// `None` means "not configured, fall back to the project's own remotes";
    /// `Some(empty)` means "explicitly nothing", which denies.
    #[serde(default)]
    pub repos: Option<BTreeSet<String>>,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            access: Access::Push,
            paths: Vec::new(),
            repos: None,
        }
    }
}

/// Why a request was refused. Rendered into the 403 body and into the
/// `credential` event's `reason`, so it must never quote a header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    Method { method: String, allowed: String },
    Path { path: String },
    Repo { repo: String },
    NoRepo,
    NoRemotes,
}

impl std::fmt::Display for Denial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Denial::Method { method, allowed } => write!(
                f,
                "method {method} is outside this provider's scope (allowed: {allowed})"
            ),
            Denial::Path { path } => {
                write!(f, "path {path} is outside this provider's allowed prefixes")
            }
            Denial::Repo { repo } => write!(
                f,
                "repository {repo} is not in this box's scope; widen it with \
                 `devbox secret scope github --repo {repo}`"
            ),
            Denial::NoRepo => write!(
                f,
                "this request names no repository, and the github scope is limited to \
                 a repository allowlist"
            ),
            Denial::NoRemotes => write!(
                f,
                "no github repository is in scope: the project directory has no github \
                 remote, so the default allowlist is empty. Add one with \
                 `devbox secret scope github --repo owner/name`"
            ),
        }
    }
}

impl Scope {
    /// Check a request against this scope.
    ///
    /// `repo` is the `owner/name` the request names, already extracted by
    /// [`repo_from_path`]; `default_repos` is the project's own remote set,
    /// used when the scope names none of its own.
    pub fn check(
        &self,
        method: &str,
        path: &str,
        repo: Option<&str>,
        github: bool,
        default_repos: &BTreeSet<String>,
    ) -> Result<(), Denial> {
        let method_upper = method.to_ascii_uppercase();
        if !self.access.methods().contains(&method_upper.as_str()) {
            return Err(Denial::Method {
                method: method_upper,
                allowed: self.access.methods().join(", "),
            });
        }

        if !self.paths.is_empty() && !self.paths.iter().any(|p| path.starts_with(p.as_str())) {
            return Err(Denial::Path {
                path: path.to_string(),
            });
        }

        if github {
            let allowed = self.repos.as_ref().unwrap_or(default_repos);
            match repo {
                Some(repo) => {
                    let repo = repo.to_ascii_lowercase();
                    if !allowed.contains(&repo) {
                        if allowed.is_empty() && self.repos.is_none() {
                            return Err(Denial::NoRemotes);
                        }
                        return Err(Denial::Repo { repo });
                    }
                }
                None => {
                    // A github request that names no repository — `/user`,
                    // `/rate_limit`, GraphQL — cannot be checked against a
                    // repository allowlist, so it is refused rather than
                    // waved through. Anything genuinely needed here belongs
                    // in `paths`, where the operator states it on purpose.
                    return Err(Denial::NoRepo);
                }
            }
        }

        Ok(())
    }
}

/// Pull `owner/name` out of a GitHub URL path.
///
/// Two shapes reach the broker, and both are covered by tests because both
/// were observed rather than guessed:
///
/// - the REST API: `/repos/{owner}/{name}/...`
/// - git smart HTTP: `/{owner}/{name}.git/info/refs`, `/{owner}/{name}.git/git-upload-pack`
///
/// Anything else yields `None`, which the scope check treats as a denial.
pub fn repo_from_path(path: &str) -> Option<String> {
    let path = path.strip_prefix('/').unwrap_or(path);
    let mut parts = path.split('/').filter(|s| !s.is_empty());
    let first = parts.next()?;

    if first == "repos" {
        let owner = parts.next()?;
        let name = parts.next()?;
        return normalize_repo(owner, name);
    }

    // Smart HTTP. The second segment is the repository, usually with `.git`,
    // and the third is one of git's three endpoints. Requiring the third is
    // what stops `/anything/else` from being read as a repository.
    let name = parts.next()?;
    let endpoint = parts.next()?;
    if matches!(endpoint, "info" | "git-upload-pack" | "git-receive-pack") {
        return normalize_repo(first, name);
    }
    None
}

fn normalize_repo(owner: &str, name: &str) -> Option<String> {
    let name = name.strip_suffix(".git").unwrap_or(name);
    if owner.is_empty() || name.is_empty() || owner.starts_with('.') || name.starts_with('.') {
        return None;
    }
    Some(format!(
        "{}/{}",
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    ))
}

/// The GitHub repositories a project directory already points at.
///
/// Parsed from `git remote -v` rather than `.git/config` so worktrees,
/// submodule checkouts, and `insteadOf` rewrites all report what git itself
/// would use. A directory that is not a repository, or a git that fails,
/// yields an empty set — deny, not allow.
pub fn project_repos(project_dir: &Path) -> BTreeSet<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "-v"])
        .current_dir(project_dir)
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        return BTreeSet::new();
    };
    if !output.status.success() {
        return BTreeSet::new();
    }
    parse_remotes(&String::from_utf8_lossy(&output.stdout))
}

/// Extract `owner/name` for every github.com remote in `git remote -v` output.
pub fn parse_remotes(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter_map(repo_from_remote_url)
        .collect()
}

/// Parse one remote URL into `owner/name`, for github.com only.
///
/// The four shapes git accepts and people actually use:
///
/// - `https://github.com/owner/repo.git`
/// - `https://user@github.com/owner/repo`
/// - `git@github.com:owner/repo.git`  (scp-like, no scheme, `:` not `/`)
/// - `ssh://git@github.com/owner/repo.git`
pub fn repo_from_remote_url(url: &str) -> Option<String> {
    let url = url.trim();

    // scp-like: `[user@]host:path`, distinguished from a URL by having no
    // `://`. Checked first because `git@github.com:owner/repo` also contains
    // a `/` and would otherwise be mis-split.
    if !url.contains("://")
        && let Some((hostish, path)) = url.split_once(':')
    {
        let host = hostish.rsplit('@').next().unwrap_or(hostish);
        if !is_github_host(host) {
            return None;
        }
        let mut parts = path.split('/').filter(|s| !s.is_empty());
        let owner = parts.next()?;
        let name = parts.next()?;
        return normalize_repo(owner, name);
    }

    let rest = url.split_once("://").map(|(_, rest)| rest)?;
    let (authority, path) = rest.split_once('/')?;
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    if !is_github_host(host) {
        return None;
    }
    let mut parts = path.split('/').filter(|s| !s.is_empty());
    let owner = parts.next()?;
    let name = parts.next()?;
    normalize_repo(owner, name)
}

fn is_github_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("www.github.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repos(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn access_levels_widen_monotonically() {
        assert!(Access::Read.methods().contains(&"GET"));
        assert!(!Access::Read.methods().contains(&"POST"));
        assert!(Access::Push.methods().contains(&"POST"));
        assert!(!Access::Push.methods().contains(&"DELETE"));
        assert!(Access::Admin.methods().contains(&"DELETE"));
        for level in [Access::Read, Access::Push, Access::Admin] {
            assert_eq!(level.as_str().parse::<Access>().unwrap(), level);
        }
        assert!("root".parse::<Access>().is_err());
    }

    #[test]
    fn method_outside_the_level_is_denied() {
        let scope = Scope {
            access: Access::Read,
            ..Default::default()
        };
        assert!(
            scope
                .check("GET", "/v1/messages", None, false, &repos(&[]))
                .is_ok()
        );
        // Case folded: a client sending `post` gets the same answer as `POST`.
        let denial = scope
            .check("post", "/v1/messages", None, false, &repos(&[]))
            .unwrap_err();
        assert!(matches!(denial, Denial::Method { .. }));
        assert!(denial.to_string().contains("POST"));
    }

    #[test]
    fn path_prefixes_are_a_whitelist_when_present() {
        let scope = Scope {
            access: Access::Push,
            paths: vec!["/v1/messages".into()],
            repos: None,
        };
        assert!(
            scope
                .check("POST", "/v1/messages", None, false, &repos(&[]))
                .is_ok()
        );
        assert!(
            scope
                .check("POST", "/v1/complete", None, false, &repos(&[]))
                .is_err()
        );

        let open = Scope {
            access: Access::Push,
            paths: vec![],
            repos: None,
        };
        assert!(
            open.check("POST", "/anything", None, false, &repos(&[]))
                .is_ok()
        );
    }

    #[test]
    fn github_defaults_to_the_projects_own_remotes() {
        let scope = Scope::default();
        let default = repos(&["ethannortharc/devbox"]);

        assert!(
            scope
                .check(
                    "GET",
                    "/repos/ethannortharc/devbox",
                    Some("ethannortharc/devbox"),
                    true,
                    &default
                )
                .is_ok()
        );
        let denial = scope
            .check(
                "GET",
                "/repos/someone/else",
                Some("someone/else"),
                true,
                &default,
            )
            .unwrap_err();
        assert_eq!(
            denial,
            Denial::Repo {
                repo: "someone/else".into()
            }
        );
        assert!(denial.to_string().contains("devbox secret scope github"));
    }

    #[test]
    fn a_project_with_no_github_remote_denies_and_explains() {
        let scope = Scope::default();
        let denial = scope
            .check("GET", "/repos/a/b", Some("a/b"), true, &repos(&[]))
            .unwrap_err();
        assert_eq!(denial, Denial::NoRemotes);
        assert!(denial.to_string().contains("devbox secret scope github"));
    }

    #[test]
    fn an_explicit_empty_allowlist_is_not_the_no_remotes_message() {
        let scope = Scope {
            repos: Some(BTreeSet::new()),
            ..Default::default()
        };
        let denial = scope
            .check("GET", "/repos/a/b", Some("a/b"), true, &repos(&["x/y"]))
            .unwrap_err();
        assert_eq!(denial, Denial::Repo { repo: "a/b".into() });
    }

    #[test]
    fn a_github_request_naming_no_repository_is_refused() {
        let scope = Scope::default();
        let denial = scope
            .check("GET", "/user", None, true, &repos(&["a/b"]))
            .unwrap_err();
        assert_eq!(denial, Denial::NoRepo);
    }

    #[test]
    fn repositories_come_out_of_both_api_and_smart_http_paths() {
        assert_eq!(
            repo_from_path("/repos/ethannortharc/devbox/pulls").as_deref(),
            Some("ethannortharc/devbox")
        );
        // The exact path git sent in the measurement, `.git` suffix and all.
        assert_eq!(
            repo_from_path("/ethannortharc/devbox.git/info/refs").as_deref(),
            Some("ethannortharc/devbox")
        );
        assert_eq!(
            repo_from_path("/ethannortharc/devbox.git/git-upload-pack").as_deref(),
            Some("ethannortharc/devbox")
        );
        assert_eq!(
            repo_from_path("/ethannortharc/devbox/git-receive-pack").as_deref(),
            Some("ethannortharc/devbox")
        );
        // Case is folded so `Owner/Repo` and `owner/repo` are one entry.
        assert_eq!(
            repo_from_path("/repos/EthanNorthArc/DevBox").as_deref(),
            Some("ethannortharc/devbox")
        );

        for none in ["/user", "/", "/rate_limit", "/graphql", "/repos/only-owner"] {
            assert_eq!(repo_from_path(none), None, "{none}");
        }
    }

    #[test]
    fn remote_urls_parse_in_every_shape_git_accepts() {
        let cases = [
            (
                "https://github.com/ethannortharc/devbox.git",
                Some("ethannortharc/devbox"),
            ),
            (
                "https://github.com/ethannortharc/devbox",
                Some("ethannortharc/devbox"),
            ),
            (
                "https://user@github.com/Ethan/Devbox.git",
                Some("ethan/devbox"),
            ),
            (
                "git@github.com:ethannortharc/devbox.git",
                Some("ethannortharc/devbox"),
            ),
            (
                "git@github.com:ethannortharc/devbox",
                Some("ethannortharc/devbox"),
            ),
            (
                "ssh://git@github.com/ethannortharc/devbox.git",
                Some("ethannortharc/devbox"),
            ),
            // Not github: a self-hosted forge must not inherit the github scope.
            ("git@gitlab.com:ethannortharc/devbox.git", None),
            ("https://gitea.example.com/ethannortharc/devbox.git", None),
            // A lookalike host. `github.com.evil.test` is not github.com.
            ("https://github.com.evil.test/a/b.git", None),
            ("/local/path/repo.git", None),
            ("", None),
        ];
        for (url, want) in cases {
            assert_eq!(
                repo_from_remote_url(url).as_deref(),
                want,
                "remote url {url:?}"
            );
        }
    }

    #[test]
    fn git_remote_v_output_collapses_fetch_and_push_into_one_entry() {
        let text = "\
origin\thttps://github.com/ethannortharc/devbox.git (fetch)
origin\thttps://github.com/ethannortharc/devbox.git (push)
upstream\tgit@github.com:someone/devbox.git (fetch)
upstream\tgit@github.com:someone/devbox.git (push)
mirror\thttps://gitea.example.com/x/y.git (fetch)
";
        assert_eq!(
            parse_remotes(text),
            repos(&["ethannortharc/devbox", "someone/devbox"])
        );
        assert!(parse_remotes("").is_empty());
    }

    #[test]
    fn scope_round_trips_through_json() {
        let scope = Scope {
            access: Access::Admin,
            paths: vec!["/repos/".into()],
            repos: Some(repos(&["a/b"])),
        };
        let json = serde_json::to_string(&scope).unwrap();
        assert_eq!(serde_json::from_str::<Scope>(&json).unwrap(), scope);
        // A file written before `paths`/`repos` existed still loads.
        let minimal: Scope = serde_json::from_str(r#"{"access":"read"}"#).unwrap();
        assert_eq!(minimal.access, Access::Read);
        assert!(minimal.paths.is_empty());
        assert!(minimal.repos.is_none());
    }
}
