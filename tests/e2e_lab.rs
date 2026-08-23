//! End-to-end test: a **real lab**, wired inside a real Linux substrate.
//!
//! Builds a privileged container with `iproute2`, runs the generated wiring
//! commands verbatim, starts the generated FRR configuration, waits for every
//! BGP adjacency, and checks the routed loopback reachability matrix (§9.2).
//!
//! Skips when Docker is unavailable, or when the daemon refuses the privileges
//! network namespaces require.

use std::process::Command;

use devbox::lab::fault::{Direction, Impairment};
use devbox::lab::{Lab, fault, wiring};

const CONTAINER: &str = "devbox-e2e-lab";
const IMAGE: &str = "devbox-e2e-lab:routed-v3";

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build a substrate with the tools and real routing daemon a lab needs.
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
        // `sudo` because generated commands carry it; symlinks because Debian
        // keeps daemons in /usr/lib/frr while a Nix network set puts them on
        // PATH. The test exercises devbox's argv, not a distro path accident.
        "FROM debian:bookworm-slim\n\
         RUN apt-get update && apt-get install -y --no-install-recommends \\\n             iproute2 iputils-ping procps sudo frr ca-certificates && \\\n             ln -s /usr/lib/frr/zebra /usr/local/bin/zebra && \\\n             ln -s /usr/lib/frr/bgpd /usr/local/bin/bgpd && \\\n             rm -rf /var/lib/apt/lists/*\n\
         CMD [\"sleep\", \"infinity\"]\n",
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

fn established_count(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(value) if value == "Established" => 1,
        serde_json::Value::Array(values) => values.iter().map(established_count).sum(),
        serde_json::Value::Object(values) => values.values().map(established_count).sum(),
        _ => 0,
    }
}

fn install_router_configs(lab: &Lab, scratch: &std::path::Path) {
    for command in devbox::lab::frr::identity_commands() {
        let (code, output) = exec(&command);
        assert_eq!(
            code,
            0,
            "prepare FRR runtime identity with `{}`: {output}",
            wiring::render(&command)
        );
    }
    for (node, config) in lab.router_configs() {
        let directory = format!("/etc/devbox/lab/{}/{node}", lab.name());
        let (code, output) = exec(&["mkdir".into(), "-p".into(), directory.clone()]);
        assert_eq!(code, 0, "create router config directory: {output}");

        let source = scratch.join(format!("{node}-frr.conf"));
        std::fs::write(&source, config).expect("write host-side FRR config");
        let destination = format!("{CONTAINER}:{directory}/frr.conf");
        let copied = Command::new("docker")
            .args(["cp", &source.display().to_string(), &destination])
            .output()
            .expect("docker cp router config");
        assert!(
            copied.status.success(),
            "copy {node} config: {}",
            String::from_utf8_lossy(&copied.stderr)
        );

        for command in devbox::lab::frr::start_commands(lab.name(), &node) {
            let (code, output) = exec(&command);
            assert_eq!(code, 0, "`{}` failed: {output}", wiring::render(&command));
        }
    }
}

fn wait_for_bgp(lab: &Lab) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut latest = std::collections::BTreeMap::new();
    loop {
        let mut ready = true;
        for node in lab.topology.routers() {
            let (code, output) = exec(&devbox::lab::frr::convergence_check(lab.name(), &node.name));
            let expected = lab.topology.routing_interfaces_of(&node.name).len();
            let established = serde_json::from_str::<serde_json::Value>(&output)
                .ok()
                .map(|value| established_count(&value))
                .unwrap_or(0);
            let (_, rib) = exec(&wiring::in_node(
                lab.name(),
                &node.name,
                &[
                    "vtysh",
                    "-N",
                    &wiring::netns(lab.name(), &node.name),
                    "-c",
                    "show ip bgp json",
                ],
            ));
            let learned_every_loopback = lab
                .plan
                .loopbacks
                .values()
                .all(|address| rib.contains(&address.to_string()));
            latest.insert(
                node.name.clone(),
                format!(
                    "exit={code}, established={established}/{expected}, \
                     all_loopbacks={learned_every_loopback}: {output}\n{rib}"
                ),
            );
            if code != 0 || established != expected || !learned_every_loopback {
                ready = false;
                break;
            }
        }
        if ready {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "BGP did not converge before the deadline:\n{latest:#?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[test]
fn a_real_lab_routes_every_node_and_survives_faults() {
    if !docker_available() {
        eprintln!("skipping: docker is not available");
        return;
    }
    if !ensure_image() {
        panic!("docker is available but the test image {IMAGE} could not be built");
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

    // ── real FRR, real BGP convergence ───────────────────
    let scratch = tempfile::tempdir().expect("FRR config scratch directory");
    install_router_configs(&lab, scratch.path());
    wait_for_bgp(&lab);

    // Loopbacks require learned routes. Every ordered pair must now work,
    // including leaf1 ↔ leaf2 across the spine.
    let mut routed = 0;
    for (from, to, command) in lab.reachability_commands() {
        let (code, output) = exec(&command);
        let diagnostics = if code == 0 {
            String::new()
        } else {
            let mut detail = String::new();
            for node in lab.topology.routers() {
                let (_, routes) = exec(&wiring::in_node(
                    lab.name(),
                    &node.name,
                    &["ip", "-4", "route", "show"],
                ));
                let (_, bgp) = exec(&wiring::in_node(
                    lab.name(),
                    &node.name,
                    &[
                        "vtysh",
                        "-N",
                        &wiring::netns(lab.name(), &node.name),
                        "-c",
                        "show ip bgp",
                        "-c",
                        "show bgp neighbor",
                    ],
                ));
                detail.push_str(&format!(
                    "\n{} kernel routes:\n{routes}\n{} BGP:\n{bgp}",
                    node.name, node.name
                ));
            }
            detail
        };
        assert_eq!(
            code, 0,
            "routed reachability {from} → {to} failed:\n{output}{diagnostics}"
        );
        routed += 1;
    }
    assert_eq!(routed, 6, "three routers require six ordered checks");

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
