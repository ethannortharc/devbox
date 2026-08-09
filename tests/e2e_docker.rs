//! End-to-end test: the console driving a **real box**.
//!
//! Everything else in the suite stubs the runtime out. This one creates an
//! actual Docker container through the `Runtime` trait, registers it as a box,
//! serves the console on a real socket, and drives the full Phase-1 surface
//! over HTTP and WebSocket:
//!
//!   create → list → start → status → files → terminal (pty over ws) → stop → destroy
//!
//! It skips (rather than fails) when Docker is unavailable, so `cargo test` on
//! a machine without Docker stays green. CI always has Docker, so it always
//! runs there.
//!
//! The base image is overridden with `DEVBOX_DOCKER_IMAGE` because the default
//! `devbox-nixos:latest` must be built locally and is not on any registry;
//! this test only needs a container that stays up and has a shell.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use devbox::runtime::docker::DockerRuntime;
use devbox::runtime::{CreateOpts, Runtime, SandboxStatus};
use devbox::sandbox::SandboxManager;
use devbox::sandbox::state::SandboxState;
use devbox::web::routes;
use devbox::web::state::AppState;
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite;

const TOKEN: &str = "e2e-token";
const BOX: &str = "e2e-console";

/// Base image the test builds locally.
///
/// `DockerRuntime::create` runs the image with no command, so the image itself
/// has to stay up — exactly like the real `devbox-nixos` image, which runs an
/// init. busybox is a few megabytes and its `sh` is enough to prove the pty
/// bridge end to end.
const IMAGE: &str = "devbox-e2e-base:latest";
const BASE: &str = "busybox:stable";

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build the long-running base image. Returns false if the base cannot be
/// fetched (offline runner), which makes the test skip rather than fail.
fn ensure_image() -> bool {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", IMAGE])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if present {
        return true;
    }

    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => return false,
    };
    let dockerfile = dir.path().join("Dockerfile");
    if std::fs::write(
        &dockerfile,
        format!("FROM {BASE}\nCMD [\"sleep\", \"infinity\"]\n"),
    )
    .is_err()
    {
        return false;
    }

    std::process::Command::new("docker")
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

fn create_opts(project: &std::path::Path) -> CreateOpts {
    CreateOpts {
        name: BOX.to_string(),
        mounts: vec![devbox::runtime::Mount {
            host_path: project.to_path_buf(),
            container_path: "/mnt/host".to_string(),
            read_only: true,
        }],
        cpu: 0,
        memory: String::new(),
        env: Default::default(),
        env_file: None,
        sets: vec![],
        tools: vec![],
        bare: true,
        writable: false,
        image: "busybox".to_string(),
    }
}

/// Serve the console on an ephemeral loopback port; returns its address.
async fn serve_console(state_dir: PathBuf) -> SocketAddr {
    let manager = Arc::new(SandboxManager { state_dir });
    let app = routes::router(AppState::new(manager, TOKEN));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

fn client(port: u16) -> reqwest_lite::Client {
    reqwest_lite::Client::new(TOKEN, port)
}

/// A tiny HTTP client over `hyper`'s low-level API would be a lot of code, so
/// the test shells out to `curl`, which is present on every runner this test
/// can run on (it needs Docker anyway).
mod reqwest_lite {
    pub struct Client {
        /// The `Cookie` header *value*, built once.
        ///
        /// The console names its session cookie after the port it is serving
        /// on, because cookies are not port-scoped and two consoles would
        /// otherwise evict each other. This client binds an ephemeral port, so
        /// the name is not a constant — and when round 31 introduced the
        /// scoping it updated the in-process tests and not this one, which
        /// skips when Docker is absent. It therefore stayed green locally
        /// while every request in it would have 401'd on CI.
        cookie: String,
    }

    pub struct Res {
        pub status: u16,
        pub body: String,
    }

    impl Client {
        pub fn new(token: &str, port: u16) -> Self {
            Self {
                cookie: format!("devbox_console_{port}={token}"),
            }
        }

        /// The header value, for the WebSocket client that builds its own
        /// request. One definition, so the two cannot drift.
        pub fn cookie(&self) -> &str {
            &self.cookie
        }

        pub fn request(&self, method: &str, url: &str) -> Res {
            let out = std::process::Command::new("curl")
                .args([
                    "-sS",
                    "-X",
                    method,
                    "-H",
                    &format!("Cookie: {}", self.cookie),
                    "-w",
                    "\n%{http_code}",
                    url,
                ])
                .output()
                .expect("curl runs");
            let text = String::from_utf8_lossy(&out.stdout);
            let (body, status) = text.rsplit_once('\n').unwrap_or(("", "0"));
            Res {
                status: status.trim().parse().unwrap_or(0),
                body: body.to_string(),
            }
        }

        pub fn get(&self, url: &str) -> Res {
            self.request("GET", url)
        }

        pub fn post(&self, url: &str) -> Res {
            self.request("POST", url)
        }

        pub fn post_form(&self, url: &str, form: &str) -> Res {
            let out = std::process::Command::new("curl")
                .args([
                    "-sS",
                    "-X",
                    "POST",
                    "-H",
                    &format!("Cookie: {}", self.cookie),
                    "-H",
                    "Content-Type: application/x-www-form-urlencoded",
                    "--data",
                    form,
                    "-w",
                    "\n%{http_code}",
                    url,
                ])
                .output()
                .expect("curl runs");
            let text = String::from_utf8_lossy(&out.stdout);
            let (body, status) = text.rsplit_once('\n').unwrap_or(("", "0"));
            Res {
                status: status.trim().parse().unwrap_or(0),
                body: body.to_string(),
            }
        }

        /// Open an SSE stream and start collecting it in the background.
        ///
        /// Subscribing *before* triggering the work is what the real page does
        /// — the console connects its stream on page load, long before any
        /// form is submitted — and it is the only way to see the first events.
        pub fn start_stream(&self, url: &str) -> Stream {
            use std::io::Read;

            let mut child = std::process::Command::new("curl")
                .args(["-sS", "-N", "-H", &format!("Cookie: {}", self.cookie), url])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("curl runs");

            let mut stdout = child.stdout.take().expect("piped stdout");
            let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let sink = seen.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stdout.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut acc) = sink.lock() {
                        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
            });

            Stream { child, seen }
        }
    }

    /// A live SSE subscription.
    pub struct Stream {
        child: std::process::Child,
        seen: std::sync::Arc<std::sync::Mutex<String>>,
    }

    impl Stream {
        /// Wait until `done` accepts what has arrived, or the timeout expires.
        /// Returns everything seen either way.
        pub fn wait_for(
            &self,
            timeout: std::time::Duration,
            done: impl Fn(&str) -> bool,
        ) -> String {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                let snapshot = self.seen.lock().map(|s| s.clone()).unwrap_or_default();
                if done(&snapshot) || std::time::Instant::now() >= deadline {
                    return snapshot;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    impl Drop for Stream {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Removes the container on the way out, including during a panic unwind, so
/// a failing assertion never leaves a container behind.
struct ContainerGuard;

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &format!("devbox-{BOX}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn console_drives_a_real_docker_box_end_to_end() {
    if !docker_available() {
        eprintln!("skipping: docker is not available");
        return;
    }
    if !ensure_image() {
        eprintln!("skipping: could not obtain {IMAGE}");
        return;
    }

    // SAFETY: the test binary is single-purpose and this runs before any
    // thread reads the variable.
    unsafe { std::env::set_var(DockerRuntime::IMAGE_ENV, IMAGE) };

    let runtime = DockerRuntime;
    let state_dir = tempfile::tempdir().expect("state dir");
    let project = tempfile::tempdir().expect("project dir");

    // Clean up any leftovers from a previous failed run, and guarantee cleanup
    // on the way out.
    let _ = runtime.destroy(BOX).await;
    let _guard = ContainerGuard;

    // ── create ───────────────────────────────────────────
    runtime
        .create(&create_opts(project.path()))
        .await
        .expect("docker container is created");

    // Registering the box is what `create_sandbox` does after provisioning;
    // this test is about the console, not about Nix provisioning.
    SandboxState {
        package_sources: Default::default(),
        name: BOX.to_string(),
        runtime: "docker".to_string(),
        project_dir: project.path().to_path_buf(),
        created_at: "2026-08-06T00:00:00Z".to_string(),
        mount_mode: "overlay".to_string(),
        sets: vec!["system".to_string()],
        languages: vec![],
        image: "busybox".to_string(),
        packages: vec![],
    }
    .save(state_dir.path())
    .expect("box is registered");

    let addr = serve_console(state_dir.path().to_path_buf()).await;
    let base = format!("http://127.0.0.1:{}", addr.port());
    let http = client(addr.port());

    // ── list shows it running ────────────────────────
    let res = http.get(&format!("{base}/api/boxes"));
    assert_eq!(res.status, 200, "list: {}", res.body);
    let json: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(json[0]["name"], BOX);
    assert_eq!(json[0]["status"], "running", "a fresh container is running");

    // ── stop through the API ─────────────────────────
    let res = http.post(&format!("{base}/api/boxes/{BOX}/stop"));
    assert_eq!(res.status, 200, "stop: {}", res.body);
    assert!(res.body.contains("status-stopped"), "card: {}", res.body);
    assert_eq!(
        runtime.status(BOX).await.unwrap(),
        SandboxStatus::Stopped,
        "the runtime really stopped it"
    );

    // ── start through the API ────────────────────────
    let res = http.post(&format!("{base}/api/boxes/{BOX}/start"));
    assert_eq!(res.status, 200, "start: {}", res.body);
    assert!(res.body.contains("status-running"));
    assert_eq!(runtime.status(BOX).await.unwrap(), SandboxStatus::Running);

    // ── detail page and files tab render ─────────────
    let res = http.get(&format!("{base}/boxes/{BOX}?tab=files"));
    assert_eq!(res.status, 200);
    assert!(res.body.contains(BOX));

    let res = http.get(&format!("{base}/api/boxes/{BOX}/files"));
    assert_eq!(res.status, 200, "files: {}", res.body);

    // ── interactive terminal over a real pty ─────────
    let ws_url = format!("ws://127.0.0.1:{}/api/boxes/{BOX}/term", addr.port());
    let request = tungstenite::http::Request::builder()
        .uri(&ws_url)
        .header("Host", format!("127.0.0.1:{}", addr.port()))
        .header("Cookie", http.cookie())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .unwrap();

    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("terminal websocket connects");

    // Ask the shell to say something unmistakable.
    socket
        .send(tungstenite::Message::Binary(
            b"echo devbox-e2e-marker\n".to_vec().into(),
        ))
        .await
        .expect("keystrokes reach the pty");

    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(5), socket.next()).await
        else {
            break;
        };
        if let tungstenite::Message::Binary(bytes) = msg {
            seen.push_str(&String::from_utf8_lossy(&bytes));
            // The echo of the command itself also contains the marker, so
            // require it twice: once echoed, once as output.
            if seen.matches("devbox-e2e-marker").count() >= 2 {
                break;
            }
        }
    }
    assert!(
        seen.contains("devbox-e2e-marker"),
        "terminal produced no marker; saw: {seen:?}"
    );
    let _ = socket.close(None).await;

    // ── set checklist reports the real outcome over SSE ──
    // busybox has no nixos-rebuild. What this asserts is that the request is
    // accepted, the work really starts against the box, and the outcome — here
    // the actionable "this is not a NixOS box" guidance, rather than a doomed
    // rebuild that half-applies files first — reaches the SSE channel.
    // Subscribe first, exactly as the page does.
    let stream = http.start_stream(&format!("{base}/api/stream"));
    stream.wait_for(Duration::from_secs(5), |s| s.contains("event: tick"));

    let res = http.post_form(
        &format!("{base}/api/boxes/{BOX}/sets"),
        "set=system&set=git",
    );
    assert_eq!(res.status, 202, "apply sets: {}", res.body);
    assert!(res.body.contains("sse-swap=\"build-e2e-console\""));

    let seen = stream.wait_for(Duration::from_secs(30), |s| {
        s.contains("build-status-e2e-console")
    });
    assert!(
        seen.contains("build-status-e2e-console"),
        "no terminal build status; saw: {seen}"
    );
    assert!(
        seen.contains("nixos-rebuild"),
        "a non-NixOS box must be told why, not handed an opaque failure; saw: {seen}"
    );
    assert!(
        seen.contains("devbox nix"),
        "the message must name the command that does work here; saw: {seen}"
    );
    drop(stream);

    // ── destroy through the API ──────────────────────
    let res = http.post(&format!("{base}/api/boxes/{BOX}/destroy"));
    assert_eq!(res.status, 200, "destroy: {}", res.body);
    assert_eq!(runtime.status(BOX).await.unwrap(), SandboxStatus::NotFound);

    let res = http.get(&format!("{base}/api/boxes"));
    let json: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert!(
        json.as_array().unwrap().is_empty(),
        "the box is gone from state too"
    );
}
