//! End-to-end test: a **real lab**, wired inside a real Linux substrate.
//!
//! Builds a privileged container with `iproute2`, runs the generated wiring
//! commands verbatim, and then checks the result the way §9.2 says to: a
//! reachability matrix.
//!
//! Scope, stated honestly: this proves the *wiring* — namespaces, veth pairs,
//! addressing, link state — and therefore checks every directly-connected pair.
//! Reachability *across* the fabric needs FRR running BGP, which needs FRR in
//! the substrate image; that is the privileged Linux CI job's business. The
//! generated FRR config is covered by `src/lab/frr.rs`'s tests.
//!
//! Skips when Docker is unavailable, or when the daemon refuses the privileges
//! network namespaces require.

use std::process::Command;

use devbox::lab::fault::{Direction, Impairment};
use devbox::lab::{Lab, fault, wiring};

const CONTAINER: &str = "devbox-e2e-lab";
const IMAGE: &str = "devbox-e2e-lab:latest";

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build a small image with the one tool a lab substrate actually needs.
fn ensure_image() -> bool {
    let present = Command::new("docker")
        .args(["image", "inspect", IMAGE])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if present {
        return true;
    }

    let Ok(dir) = tempfile::tempdir() else {
        return false;
    };
    let dockerfile = dir.path().join("Dockerfile");
    if std::fs::write(
        &dockerfile,
        "FROM alpine:3\nRUN apk add --no-cache iproute2 iputils\nCMD [\"sleep\", \"infinity\"]\n",
    )
    .is_err()
    {
        return false;
    }

    Command::new("docker")
        .args([
            "build",
            "--quiet",
            "-t",
            IMAGE,
            "-f",
            &dockerfile.display().to_string(),
            &dir.path().display().to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Removes the substrate on the way out, including during a panic unwind.
struct SubstrateGuard;

impl Drop for SubstrateGuard {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", CONTAINER])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Run a command inside the substrate, returning (exit code, stdout+stderr).
fn exec(argv: &[String]) -> (i32, String) {
    let mut args: Vec<String> = vec!["exec".into(), CONTAINER.into()];
    args.extend(argv.iter().cloned());

    let out = Command::new("docker")
        .args(&args)
        .output()
        .expect("docker exec runs");

    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

#[test]
fn a_real_lab_wires_up_and_directly_connected_nodes_reach_each_other() {
    if !docker_available() {
        eprintln!("skipping: docker is not available");
        return;
    }
    if !ensure_image() {
        eprintln!("skipping: could not build {IMAGE}");
        return;
    }

    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER])
        .output();

    // Network namespaces need NET_ADMIN and a writable /run/netns.
    let started = Command::new("docker")
        .args(["run", "-d", "--name", CONTAINER, "--privileged", IMAGE])
        .output()
        .expect("docker run");
    if !started.status.success() {
        eprintln!(
            "skipping: the daemon refused a privileged container: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        return;
    }
    let _guard = SubstrateGuard;

    // Sanity: the substrate really has iproute2.
    let (code, _) = exec(&["ip".into(), "-V".into()]);
    if code != 0 {
        eprintln!("skipping: the substrate has no usable `ip`");
        return;
    }

    let lab = Lab::resolve("clos-3node").expect("the scenario resolves");

    // ── bring the wiring up, command by command ──────────
    for cmd in lab.up_commands().expect("wiring commands") {
        let (code, output) = exec(&cmd);
        assert_eq!(code, 0, "`{}` failed: {output}", wiring::render(&cmd));
    }

    // ── every namespace exists ───────────────────────────
    let (_, listed) = exec(&["ip".into(), "netns".into(), "list".into()]);
    for node in &lab.topology.nodes {
        let ns = wiring::netns(lab.name(), &node.name);
        assert!(listed.contains(&ns), "namespace {ns} is missing:\n{listed}");
    }

    // ── every planned address is on the right interface ──
    for link in &lab.plan.links {
        for end in [&link.a, &link.b] {
            let cmd = wiring::in_node(
                lab.name(),
                &end.node,
                &["ip", "-4", "addr", "show", "dev", &end.iface],
            );
            let (code, output) = exec(&cmd);
            assert_eq!(code, 0, "{}: {output}", wiring::render(&cmd));
            assert!(
                output.contains(&end.addr.to_string()),
                "{}:{} should carry {}:\n{output}",
                end.node,
                end.iface,
                end.addr
            );
            assert!(
                output.contains("state UP") || output.contains("UP,LOWER_UP"),
                "{}:{} should be up:\n{output}",
                end.node,
                end.iface
            );
        }
    }

    // ── router loopbacks landed ──────────────────────────
    for (node, addr) in &lab.plan.loopbacks {
        let cmd = wiring::in_node(lab.name(), node, &["ip", "-4", "addr", "show", "dev", "lo"]);
        let (code, output) = exec(&cmd);
        assert_eq!(code, 0, "{output}");
        assert!(
            output.contains(&addr.to_string()),
            "{node}'s loopback {addr} is missing:\n{output}"
        );
    }

    // ── the reachability matrix, for what wiring alone can deliver ──
    //
    // Without BGP, only directly-connected pairs are reachable. Every one of
    // them must be, or the wiring is wrong.
    let mut checked = 0;
    for link in &lab.plan.links {
        for (from, to) in [(&link.a, &link.b), (&link.b, &link.a)] {
            let cmd = wiring::ping(lab.name(), &from.node, &to.addr);
            let (code, output) = exec(&cmd);
            assert_eq!(
                code, 0,
                "{} cannot reach {} at {}:\n{output}",
                from.node, to.node, to.addr
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 4, "two links, both directions");

    // A node that is *not* directly connected is unreachable without routing,
    // which confirms the matrix is measuring something real rather than
    // everything being on one flat segment.
    let leaf2_addr = lab
        .plan
        .links
        .iter()
        .find(|l| l.a.node == "leaf2" || l.b.node == "leaf2")
        .map(|l| {
            if l.a.node == "leaf2" {
                l.a.addr
            } else {
                l.b.addr
            }
        })
        .expect("leaf2 has an address");
    let (code, _) = exec(&wiring::ping(lab.name(), "leaf1", &leaf2_addr));
    assert_ne!(
        code, 0,
        "leaf1 reached leaf2 without routing — the namespaces are not isolated"
    );

    // ── partition, observe the loss, then heal ───────────
    //
    // Phase 7's acceptance criterion, on a real link.
    let link = fault::find_link(&lab.topology, "leaf1-spine1").expect("the link resolves");
    let (a, b) = (&lab.plan.links[0].a, &lab.plan.links[0].b);

    for cmd in fault::partition(lab.name(), &link).expect("partition commands") {
        let (code, output) = exec(&cmd);
        assert_eq!(code, 0, "`{}` failed: {output}", wiring::render(&cmd));
    }

    let (code, output) = exec(&wiring::ping(lab.name(), &a.node, &b.addr));
    assert_ne!(
        code, 0,
        "a partitioned link should not carry traffic:\n{output}"
    );

    for cmd in fault::heal(lab.name(), &link, Direction::Both) {
        let (code, output) = exec(&cmd);
        assert_eq!(code, 0, "`{}` failed: {output}", wiring::render(&cmd));
    }

    let (code, output) = exec(&wiring::ping(lab.name(), &a.node, &b.addr));
    assert_eq!(code, 0, "healing should restore the link:\n{output}");

    // ── a one-way impairment stays one-way ───────────────
    //
    // A symmetric fault would hide exactly the asymmetries worth debugging.
    let one_way = Impairment {
        loss_pct: Some(100.0),
        ..Default::default()
    };
    for cmd in fault::apply(lab.name(), &link, Direction::A, &one_way).expect("apply") {
        let (code, output) = exec(&cmd);
        assert_eq!(code, 0, "{output}");
    }

    let (from_a, _) = exec(&wiring::ping(lab.name(), &a.node, &b.addr));
    assert_ne!(from_a, 0, "the impaired end should not get packets out");

    for cmd in fault::heal(lab.name(), &link, Direction::Both) {
        let _ = exec(&cmd);
    }
    let (code, _) = exec(&wiring::ping(lab.name(), &a.node, &b.addr));
    assert_eq!(code, 0, "healing is idempotent and complete");

    // ── teardown removes everything ──────────────────────
    for cmd in lab.down_commands() {
        let (code, output) = exec(&cmd);
        assert_eq!(code, 0, "`{}` failed: {output}", wiring::render(&cmd));
    }

    let (_, listed) = exec(&["ip".into(), "netns".into(), "list".into()]);
    for node in &lab.topology.nodes {
        let ns = wiring::netns(lab.name(), &node.name);
        assert!(
            !listed.contains(&ns),
            "namespace {ns} survived teardown:\n{listed}"
        );
    }
}
