use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=DEVBOX_OBSD_BINARY");
    println!("cargo:rerun-if-env-changed=DEVBOX_OBSD_EBPF");
    println!("cargo:rerun-if-env-changed=DEVBOX_ZTPD_BINARY");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    for path in ["go.mod", "go.sum", "agent", "ztpd", "internal"] {
        println!("cargo:rerun-if-changed={path}");
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let out = out_dir.join("devbox-obsd");
    let source = env::var_os("DEVBOX_OBSD_BINARY");
    // The override's own claim about what it contains. A source build does not
    // need to be told: the generated object is either in the tree or it is not.
    let claims_ebpf = env::var("DEVBOX_OBSD_EBPF").as_deref() == Ok("1");
    assert!(
        !claims_ebpf || source.is_some(),
        "DEVBOX_OBSD_EBPF=1 requires DEVBOX_OBSD_BINARY: a source build decides from agent/bpf/devbox_<arch>_bpfel.o"
    );
    let embedded_has_ebpf = if let Some(source) = source {
        let source = PathBuf::from(source);
        fs::copy(&source, &out).unwrap_or_else(|error| {
            panic!(
                "copy prebuilt observability agent {}: {error}",
                source.display()
            )
        });
        claims_ebpf
    } else {
        build_agent(&out)
    };

    println!("cargo:rustc-env=DEVBOX_EMBEDDED_OBSD={}", out.display());
    println!(
        "cargo:rustc-env=DEVBOX_EMBEDDED_OBSD_EBPF={}",
        if embedded_has_ebpf { "1" } else { "0" }
    );

    let ztpd_out = out_dir.join("devbox-ztpd");
    if let Some(source) = env::var_os("DEVBOX_ZTPD_BINARY") {
        let source = PathBuf::from(source);
        fs::copy(&source, &ztpd_out).unwrap_or_else(|error| {
            panic!("copy prebuilt ZTP server {}: {error}", source.display())
        });
    } else {
        build_go_binary(&ztpd_out, "./ztpd/cmd/ztpd", "ZTP server", &[]);
    }
    println!(
        "cargo:rustc-env=DEVBOX_EMBEDDED_ZTPD={}",
        ztpd_out.display()
    );
}

/// Build the in-guest agent, with the kernel probes when this checkout has the
/// object for the guest architecture.
///
/// bpf2go output is committed per architecture (see `agent/bpf/README.md`),
/// because it can only be produced on a Linux host of that architecture with
/// BTF. Before it was tracked, every build from source — which is every build
/// outside a tagged release — embedded the proc+packet agent, and the symptom
/// was not a missing feature but a *plausible* one: connect events with no
/// process attribution, no file events at all, and zero measured traffic.
///
/// Returns whether the agent that was built contains the generated loader.
fn build_agent(out: &Path) -> bool {
    let arch = guest_arch();
    let object = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("agent/bpf")
        .join(format!("devbox_{arch}_bpfel.o"));
    let tags: &[&str] = if object.exists() {
        &["ebpf"]
    } else {
        println!(
            "cargo:warning=no {} — embedding the portable proc+packet agent; \
             capture will have no process attribution and no file events",
            object.display()
        );
        &[]
    };
    build_go_binary(out, "./agent/cmd/obsd", "observability agent", tags);
    !tags.is_empty()
}

/// The guest GOARCH for this build. Guests run the host binary's own
/// architecture: devbox provisions a local VM, never a foreign one.
fn guest_arch() -> &'static str {
    match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "amd64",
        other => panic!("devbox has no Linux observability agent for Rust target {other:?}"),
    }
}

fn build_go_binary(out: &Path, package: &str, description: &str, tags: &[&str]) {
    let arch = guest_arch();
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let commit = build_commit();
    let ldflags = format!(
        "-s -w -X github.com/ethannortharc/devbox/internal/buildinfo.Version={version} \
         -X github.com/ethannortharc/devbox/internal/buildinfo.Commit={commit}"
    );

    let go_cache = out
        .parent()
        .expect("agent output has a parent")
        .join("go-cache");
    fs::create_dir_all(&go_cache).expect("create build-local Go cache");
    let mut command = Command::new("go");
    command
        .env("GOOS", "linux")
        .env("GOARCH", arch)
        .env("CGO_ENABLED", "0")
        .env("GOCACHE", go_cache)
        .args(["build", "-trimpath"]);
    if !tags.is_empty() {
        command.args(["-tags", &tags.join(",")]);
    }
    let status = command
        .args(["-ldflags", &ldflags, "-o"])
        .arg(out)
        .arg(package)
        .status()
        .unwrap_or_else(|error| panic!("run Go to build the embedded {description}: {error}"));
    assert!(
        status.success(),
        "building the embedded Linux {description} failed"
    );
}

/// The commit stamped into the guest binaries.
///
/// `GITHUB_SHA` in CI, `git` otherwise. `devbox doctor` prints this back from
/// the agent's own `-version`, and "unknown" for every locally built agent made
/// that line useless for telling two builds apart.
///
/// It names the commit at the last *rebuild*, not at HEAD. This script reruns
/// when the agent's own inputs change (`agent`, `internal`, `go.mod`,
/// `go.sum`), so a commit touching only Rust leaves both the stamp and the
/// bytes it describes alone — which is the honest pairing. Forcing a rerun on
/// every commit would rebuild the agent to change nothing but its label.
fn build_commit() -> String {
    if let Some(sha) = env::var("GITHUB_SHA")
        .ok()
        .filter(|value| !value.is_empty())
    {
        return sha.chars().take(12).collect();
    }
    let described = Command::new("git")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|sha| !sha.is_empty());
    let Some(sha) = described else {
        // A source tarball, or a checkout with no `git`. Neither is an error.
        return "unknown".to_string();
    };
    let dirty = Command::new("git")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| !output.stdout.is_empty());
    if dirty { format!("{sha}-dirty") } else { sha }
}
