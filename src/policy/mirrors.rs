//! The `mirror-only` curated allowlist — §8.
//!
//! "Build, don't phone home." The posture has to let every package manager a
//! developer box actually uses do its job, and nothing else. That list is a
//! judgement call, so it lives in one place with its reasoning, not scattered
//! through the policy engine.
//!
//! Each entry is a suffix match: `pypi.org` covers `files.pypi.org`, and
//! `github.com` covers `codeload.github.com`. Entries are the *download*
//! hosts, not the web UIs, wherever the two differ.

/// A package ecosystem and the hosts it needs.
pub struct Ecosystem {
    pub name: &'static str,
    pub hosts: &'static [&'static str],
}

/// Every ecosystem `mirror-only` permits.
///
/// Deliberately conservative: telemetry endpoints, analytics, and "check for
/// updates" hosts are excluded even when the same vendor runs them, because
/// letting them through is exactly what this posture exists to prevent.
pub static ECOSYSTEMS: &[Ecosystem] = &[
    Ecosystem {
        name: "python",
        hosts: &["pypi.org", "pythonhosted.org", "files.pythonhosted.org"],
    },
    Ecosystem {
        name: "node",
        hosts: &["registry.npmjs.org", "registry.yarnpkg.com", "npmjs.com"],
    },
    Ecosystem {
        name: "rust",
        hosts: &[
            "crates.io",
            "static.crates.io",
            "index.crates.io",
            "rust-lang.org",
        ],
    },
    Ecosystem {
        name: "go",
        hosts: &["proxy.golang.org", "sum.golang.org", "golang.org", "go.dev"],
    },
    Ecosystem {
        name: "nix",
        hosts: &["cache.nixos.org", "nixos.org", "channels.nixos.org"],
    },
    Ecosystem {
        name: "ruby",
        hosts: &["rubygems.org", "index.rubygems.org"],
    },
    Ecosystem {
        name: "java",
        hosts: &[
            "repo.maven.apache.org",
            "repo1.maven.org",
            "plugins.gradle.org",
        ],
    },
    Ecosystem {
        name: "containers",
        hosts: &[
            "registry-1.docker.io",
            "auth.docker.io",
            "production.cloudflare.docker.com",
            "ghcr.io",
            "quay.io",
        ],
    },
    Ecosystem {
        name: "git",
        hosts: &[
            "github.com",
            "githubusercontent.com",
            "codeload.github.com",
            "gitlab.com",
            "bitbucket.org",
            "sr.ht",
        ],
    },
    Ecosystem {
        name: "linux-distros",
        hosts: &[
            "deb.debian.org",
            "security.debian.org",
            "archive.ubuntu.com",
            "security.ubuntu.com",
            "dl-cdn.alpinelinux.org",
        ],
    },
];

/// Whether `mirror-only` permits a host.
///
/// Matching is by suffix on label boundaries, so `evilgithub.com` does not
/// match `github.com` — the same rule the explicit allowlist uses.
pub fn permits(domain: &str) -> bool {
    let domain = domain.trim_end_matches('.');
    if domain.is_empty() {
        return false;
    }

    ECOSYSTEMS
        .iter()
        .flat_map(|e| e.hosts.iter())
        .any(|host| super::suffix_match(domain, host))
}

/// The ecosystem a host belongs to, for explaining a decision.
pub fn ecosystem_of(domain: &str) -> Option<&'static str> {
    let domain = domain.trim_end_matches('.');
    ECOSYSTEMS
        .iter()
        .find(|e| e.hosts.iter().any(|host| super::suffix_match(domain, host)))
        .map(|e| e.name)
}

/// Every permitted host, flattened and sorted — what the agent resolves into
/// the nftables set.
pub fn all_hosts() -> Vec<&'static str> {
    let mut hosts: Vec<&'static str> = ECOSYSTEMS
        .iter()
        .flat_map(|e| e.hosts.iter())
        .copied()
        .collect();
    hosts.sort_unstable();
    hosts.dedup();
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ecosystem_a_dev_box_uses_is_covered() {
        for (host, ecosystem) in [
            ("pypi.org", "python"),
            ("files.pythonhosted.org", "python"),
            ("registry.npmjs.org", "node"),
            ("crates.io", "rust"),
            ("static.crates.io", "rust"),
            ("proxy.golang.org", "go"),
            ("cache.nixos.org", "nix"),
            ("rubygems.org", "ruby"),
            ("repo1.maven.org", "java"),
            ("registry-1.docker.io", "containers"),
            ("github.com", "git"),
            ("deb.debian.org", "linux-distros"),
        ] {
            assert!(permits(host), "{host} must be permitted");
            assert_eq!(ecosystem_of(host), Some(ecosystem));
        }
    }

    #[test]
    fn subdomains_of_a_permitted_host_are_permitted() {
        assert!(permits("codeload.github.com"));
        assert!(permits("raw.githubusercontent.com"));
        assert!(permits("files.pythonhosted.org"));
    }

    #[test]
    fn lookalikes_are_not_permitted() {
        // The whole point of the posture is that this fails.
        for host in [
            "evilgithub.com",
            "github.com.evil.example",
            "pypi.org.attacker.net",
            "notcrates.io",
        ] {
            assert!(!permits(host), "{host} must not be permitted");
        }
    }

    #[test]
    fn telemetry_and_arbitrary_hosts_are_not_permitted() {
        for host in [
            "telemetry.example.com",
            "api.openai.com",
            "analytics.google.com",
            "example.com",
            "",
        ] {
            assert!(!permits(host), "{host:?} must not be permitted");
        }
    }

    #[test]
    fn matching_is_case_insensitive_and_tolerates_a_trailing_dot() {
        assert!(permits("PyPI.ORG"));
        assert!(permits("pypi.org."));
        assert!(permits("Codeload.GitHub.com."));
    }

    #[test]
    fn the_host_list_is_flattened_sorted_and_unique() {
        let hosts = all_hosts();
        assert!(!hosts.is_empty());

        let mut sorted = hosts.clone();
        sorted.sort_unstable();
        assert_eq!(hosts, sorted);

        let unique: std::collections::BTreeSet<_> = hosts.iter().collect();
        assert_eq!(unique.len(), hosts.len());
    }

    #[test]
    fn every_listed_host_is_a_plausible_domain() {
        for host in all_hosts() {
            assert!(
                super::super::is_plausible_domain(host),
                "{host} is not a valid domain"
            );
        }
    }

    #[test]
    fn an_unknown_host_has_no_ecosystem() {
        assert_eq!(ecosystem_of("example.com"), None);
    }
}
