//! End-to-end acceptance test for the flagship zero-touch fabric.
//!
//! This deliberately enters through the shipped Web control plane. The Lab
//! handlers dispatch the same `cli::lab::up`/fault/heal/down orchestration as
//! the CLI, against one privileged Docker substrate. Success therefore proves
//! the production preflight, wiring, file transfer, embedded ZTP installation,
//! DHCP options 66/67, bootstrap, FRR convergence, reachability, fault actions,
//! teardown, and the real pcap download handler rather than a test-only
//! reimplementation of those steps.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use devbox::lab::Lab;
use devbox::lab::services::{ZtpPlan, namespace, run_dir};
use devbox::sandbox::SandboxManager;
use devbox::sandbox::state::SandboxState;
use devbox::web::routes;
use devbox::web::state::AppState;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

const BOX: &str = "e2e-ztp";
const CONTAINER: &str = "devbox-e2e-ztp";
const IMAGE: &str = "devbox-e2e-ztp:v1";
const TOKEN: &str = "ztp-e2e-token";
const KEY: &str = "ztp-e2e-key";
const OPERATION: &str = "lab-ztp-fabric";

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn ensure_image() -> bool {
    if Command::new("docker")
        .args(["image", "inspect", IMAGE])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return true;
    }
    let Ok(dir) = tempfile::tempdir() else {
        return false;
    };
    let dockerfile = dir.path().join("Dockerfile");
    let contents = concat!(
        "FROM debian:bookworm-slim\n",
        "RUN apt-get update && apt-get install -y --no-install-recommends ",
        "iproute2 iputils-ping procps sudo frr dnsmasq chrony busybox wget ",
        "ca-certificates coreutils && ",
        "ln -s /usr/lib/frr/zebra /usr/local/bin/zebra && ",
        "ln -s /usr/lib/frr/bgpd /usr/local/bin/bgpd && ",
        "rm -rf /var/lib/apt/lists/*\n",
        "CMD [\"sleep\", \"infinity\"]\n",
    );
    if std::fs::write(&dockerfile, contents).is_err() {
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
        .status()
        .is_ok_and(|status| status.success())
}

struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", CONTAINER])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn exec(argv: &[&str]) -> (i32, String, String) {
    let mut args = vec!["exec", CONTAINER];
    args.extend_from_slice(argv);
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("docker exec");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn checked(argv: &[&str], action: &str) -> String {
    let (code, stdout, stderr) = exec(argv);
    assert_eq!(
        code,
        0,
        "{action} with `{}` failed:\n{stdout}{stderr}",
        argv.join(" ")
    );
    stdout
}

fn copy_bytes(scratch: &std::path::Path, name: &str, bytes: &[u8], destination: &str) {
    let source = scratch.join(name);
    std::fs::write(&source, bytes).expect("write host staging file");
    let output = Command::new("docker")
        .args([
            "cp",
            &source.display().to_string(),
            &format!("{CONTAINER}:{destination}"),
        ])
        .output()
        .expect("docker cp");
    assert!(
        output.status.success(),
        "copy {destination}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn post_form(uri: &str, form: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7878")
        .header("x-devbox-key", KEY)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_string()))
        .unwrap()
}

fn post_authed(uri: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, "127.0.0.1:7878")
        .header("x-devbox-key", KEY)
        .body(Body::empty())
        .unwrap()
}

async fn wait_for_operation(state: &AppState, timeout: Duration) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(status) = state.retained_build_status(OPERATION) {
            assert!(
                !status.contains("term-err"),
                "Lab operation failed: {status}"
            );
            if status.contains("completed") {
                return status;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Lab operation did not finish within {} seconds",
            timeout.as_secs()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn run_lab_action(app: &Router, state: &AppState, action: &str, form: &str) {
    let response = app
        .clone()
        .oneshot(post_form(&format!("/api/labs/ztp-fabric/{action}"), form))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let status = wait_for_operation(state, Duration::from_secs(200)).await;
    assert!(status.contains("term-ok"), "{status}");
}

#[derive(serde::Deserialize, Debug)]
struct Status {
    converged: bool,
    expected: usize,
    healthy: usize,
    failed: usize,
    missing: Vec<String>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_lab_blank_nodes_boot_fault_heal_capture_and_tear_down() {
    if !docker_available() {
        eprintln!("skipping: docker is not available");
        return;
    }
    assert!(ensure_image(), "could not build {IMAGE}");
    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER])
        .output();
    let started = Command::new("docker")
        .args(["run", "-d", "--name", CONTAINER, "--privileged", IMAGE])
        .output()
        .expect("docker run");
    if !started.status.success() {
        eprintln!(
            "skipping: daemon refused privileged container: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        return;
    }
    let _guard = Guard;
    let state_dir = tempfile::tempdir().expect("state directory");
    let project = tempfile::tempdir().expect("project directory");
    let scratch = tempfile::tempdir().expect("staging directory");

    SandboxState {
        schema: devbox::sandbox::state::SCHEMA,
        package_sources: Default::default(),
        name: BOX.to_string(),
        runtime: "docker".to_string(),
        project_dir: project.path().to_path_buf(),
        created_at: "2026-08-06T00:00:00Z".to_string(),
        mount_mode: "overlay".to_string(),
        sets: vec!["system".into(), "network".into()],
        languages: vec![],
        image: "ubuntu".to_string(),
        packages: vec![],
    }
    .save(state_dir.path())
    .expect("register privileged substrate");

    let manager = Arc::new(SandboxManager {
        state_dir: state_dir.path().to_path_buf(),
    });
    let app_state = AppState::new(manager, TOKEN, KEY);
    let app = routes::router(app_state.clone());
    let lab = Lab::resolve("ztp-fabric").expect("ZTP scenario");
    let ztp = ZtpPlan::from_lab(&lab).unwrap().unwrap();

    // The Web route must dispatch the production bring-up and not merely
    // render a template or replay the sequence inside this test.
    run_lab_action(&app, &app_state, "up", &format!("substrate={BOX}")).await;

    let service_ns = namespace(lab.name(), &ztp.service.node);
    let raw_status = checked(
        &[
            "ip",
            "netns",
            "exec",
            &service_ns,
            "wget",
            "-qO-",
            "http://127.0.0.1:9090/status",
        ],
        "read converged ZTP status",
    );
    let status: Status = serde_json::from_str(&raw_status).expect("status JSON");
    assert!(status.converged);
    assert_eq!(status.expected, ztp.blanks.len());
    assert_eq!(status.healthy, ztp.blanks.len());
    assert_eq!(status.failed, 0);
    assert!(status.missing.is_empty());

    for blank in &ztp.blanks {
        let lease = checked(
            &[
                "cat",
                &format!("{}/dhcp.env", run_dir(lab.name(), &blank.node)),
            ],
            &format!("read {} DHCP lease", blank.node),
        );
        assert!(
            lease
                .lines()
                .any(|line| line == format!("boot_url={}", ztp.boot_url)),
            "{} did not receive DHCP option 67: {lease}",
            blank.node
        );
    }

    for (from, to, command) in lab.reachability_commands() {
        let refs: Vec<&str> = command.iter().map(String::as_str).collect();
        let (code, stdout, stderr) = exec(&refs);
        assert_eq!(
            code, 0,
            "post-ZTP reachability {from} -> {to} failed:\n{stdout}{stderr}"
        );
    }

    // Drive the Web fault and heal handlers and prove they affect the actual
    // namespace link rather than only returning an accepted fragment.
    let link = &lab.plan.links[0];
    let link_name = format!("{}-{}", link.a.node, link.b.node);
    run_lab_action(
        &app,
        &app_state,
        "fault",
        &format!("substrate={BOX}&link={link_name}&partition=on&direction=both"),
    )
    .await;
    let source_ns = namespace(lab.name(), &link.a.node);
    let (partitioned, _, _) = exec(&[
        "ip",
        "netns",
        "exec",
        &source_ns,
        "ping",
        "-c",
        "1",
        "-W",
        "1",
        &link.b.addr.to_string(),
    ]);
    assert_ne!(partitioned, 0, "partition did not break the selected link");

    run_lab_action(
        &app,
        &app_state,
        "heal",
        &format!("substrate={BOX}&link={link_name}"),
    )
    .await;
    checked(
        &[
            "ip",
            "netns",
            "exec",
            &source_ns,
            "ping",
            "-c",
            "1",
            "-W",
            "2",
            &link.b.addr.to_string(),
        ],
        "verify healed link",
    );

    // Exercise the actual Web pcap response. Root-namespace UDP traffic to
    // the container gateway is visible to AF_PACKET and needs no listener.
    copy_bytes(
        scratch.path(),
        "devbox-obsd",
        devbox::embedded::OBSD,
        "/usr/local/bin/devbox-obsd",
    );
    checked(
        &["chmod", "0755", "/usr/local/bin/devbox-obsd"],
        "make observability agent executable",
    );
    let gateway = checked(
        &["sh", "-c", "ip route show default | awk '{print $3; exit}'"],
        "discover container gateway",
    )
    .trim()
    .to_string();
    let traffic_gateway = gateway.clone();
    let traffic = tokio::task::spawn_blocking(move || {
        checked(
            &[
                "bash",
                "-c",
                &format!(
                    "for i in $(seq 1 20); do \
                     printf devbox-pcap > /dev/udp/{traffic_gateway}/54321; \
                     sleep 0.2; done"
                ),
            ],
            "generate UDP capture traffic",
        );
    });
    let capture = app
        .clone()
        .oneshot(post_authed(&format!(
            "/api/boxes/{BOX}/flows/pcap?proto=udp&daddr={gateway}&dport=54321&duration=5&packets=1"
        )))
        .await
        .unwrap();
    traffic.await.unwrap();
    assert_eq!(capture.status(), StatusCode::OK);
    assert_eq!(
        capture.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/vnd.tcpdump.pcap"
    );
    assert_eq!(capture.headers().get("x-devbox-packets").unwrap(), "1");
    let pcap = capture.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(devbox::obs::pcap::validate(&pcap).unwrap(), 1);

    run_lab_action(&app, &app_state, "down", &format!("substrate={BOX}")).await;
    let remaining = checked(&["ip", "netns", "list"], "list namespaces after teardown");
    assert!(
        !remaining.contains("devbox-ztp-fabric-"),
        "teardown left lab namespaces behind: {remaining}"
    );
}
