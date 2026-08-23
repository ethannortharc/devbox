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
    let claims_ebpf = env::var("DEVBOX_OBSD_EBPF").as_deref() == Ok("1");
    assert!(
        !claims_ebpf || source.is_some(),
        "DEVBOX_OBSD_EBPF=1 requires DEVBOX_OBSD_BINARY: the default source build is the portable proc+packet agent"
    );
    if let Some(source) = source {
        let source = PathBuf::from(source);
        fs::copy(&source, &out).unwrap_or_else(|error| {
            panic!(
                "copy prebuilt observability agent {}: {error}",
                source.display()
            )
        });
    } else {
        build_portable_agent(&out);
    }

    println!("cargo:rustc-env=DEVBOX_EMBEDDED_OBSD={}", out.display());
    println!(
        "cargo:rustc-env=DEVBOX_EMBEDDED_OBSD_EBPF={}",
        if claims_ebpf { "1" } else { "0" }
    );

    let ztpd_out = out_dir.join("devbox-ztpd");
    if let Some(source) = env::var_os("DEVBOX_ZTPD_BINARY") {
        let source = PathBuf::from(source);
        fs::copy(&source, &ztpd_out).unwrap_or_else(|error| {
            panic!("copy prebuilt ZTP server {}: {error}", source.display())
        });
    } else {
        build_portable_go_binary(&ztpd_out, "./ztpd/cmd/ztpd", "ZTP server");
    }
    println!(
        "cargo:rustc-env=DEVBOX_EMBEDDED_ZTPD={}",
        ztpd_out.display()
    );
}

fn build_portable_agent(out: &Path) {
    build_portable_go_binary(out, "./agent/cmd/obsd", "observability agent");
}

fn build_portable_go_binary(out: &Path, package: &str, description: &str) {
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "amd64",
        other => panic!("devbox has no Linux observability agent for Rust target {other:?}"),
    };
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let commit = env::var("GITHUB_SHA")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(12).collect::<String>())
        .unwrap_or_else(|| "unknown".to_string());
    let ldflags = format!(
        "-s -w -X github.com/ethannortharc/devbox/internal/buildinfo.Version={version} \
         -X github.com/ethannortharc/devbox/internal/buildinfo.Commit={commit}"
    );

    let go_cache = out
        .parent()
        .expect("agent output has a parent")
        .join("go-cache");
    fs::create_dir_all(&go_cache).expect("create build-local Go cache");
    let status = Command::new("go")
        .env("GOOS", "linux")
        .env("GOARCH", arch)
        .env("CGO_ENABLED", "0")
        .env("GOCACHE", go_cache)
        .args(["build", "-trimpath", "-ldflags", &ldflags, "-o"])
        .arg(out)
        .arg(package)
        .status()
        .unwrap_or_else(|error| panic!("run Go to build the embedded {description}: {error}"));
    assert!(
        status.success(),
        "building the embedded Linux {description} failed"
    );
}
