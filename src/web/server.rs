//! Console server lifecycle: bind, announce, open, serve.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::net::TcpListener;

use super::routes;
use super::state::AppState;
use crate::sandbox::SandboxManager;

/// Default console port (§6.1).
pub const DEFAULT_PORT: u16 = 7878;

/// How many consecutive ports to try before giving up. Enough headroom for a
/// handful of consoles side by side, small enough to fail fast if something is
/// squatting the whole range.
pub const PORT_SCAN: u16 = 16;

/// Options for [`serve`].
#[derive(Debug, Clone)]
pub struct WebOptions {
    /// First port to try; the next [`PORT_SCAN`] are tried if it is taken.
    pub port: u16,
    /// Open the console in the default browser once it is listening.
    pub open: bool,
    /// Path the opened URL lands on. Bare `devbox` in a project directory
    /// lands on that project's box rather than the dashboard.
    pub landing: String,
}

impl Default for WebOptions {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            open: true,
            landing: "/".to_string(),
        }
    }
}

/// Bind the first free port at or after `start`, on loopback only.
///
/// Loopback is not configurable: exposing the console off-host is explicitly a
/// non-goal (N1), and the token is not a substitute for real authentication.
pub async fn bind_loopback(start: u16, scan: u16) -> Result<(TcpListener, SocketAddr)> {
    let mut last_err = None;
    for offset in 0..scan {
        let port = start.saturating_add(offset);
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                let local = listener
                    .local_addr()
                    .context("listener has no local address")?;
                return Ok((listener, local));
            }
            Err(e) => last_err = Some((port, e)),
        }
    }
    match last_err {
        Some((port, e)) => Err(e).with_context(|| {
            format!("no free port in 127.0.0.1:{start}..={port} for the devbox console")
        }),
        None => bail!("port scan range was empty (scan={scan})"),
    }
}

/// The URL to hand the user, carrying the one-time token.
///
/// `landing` is a path such as `/` or `/boxes/myapp`; the token rides as a
/// query parameter and is exchanged for a cookie on the first request.
pub fn console_url(addr: &SocketAddr, token: &str, landing: &str) -> String {
    let path = if landing.starts_with('/') {
        landing
    } else {
        "/"
    };
    let sep = if path.contains('?') { '&' } else { '?' };
    format!(
        "http://{}:{}{path}{sep}t={token}",
        Ipv4Addr::LOCALHOST,
        addr.port()
    )
}

/// Ask the desktop to open a URL. Best-effort: a headless host simply prints
/// the URL instead, which is the documented fallback.
pub fn open_browser(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to launch `{opener}`"))?;
    Ok(())
}

/// Start the console and serve until interrupted.
pub async fn serve(manager: Arc<SandboxManager>, opts: WebOptions) -> Result<()> {
    let token = super::auth::generate_token();
    let state = AppState::new(manager, token.clone());
    let app = routes::router(state.clone());

    // Keeps open dashboards current; idles while no console is connected.
    tokio::spawn(super::watch::run(state.clone()));

    let (listener, addr) = bind_loopback(opts.port, PORT_SCAN).await?;
    let url = console_url(&addr, &token, &opts.landing);

    println!("devbox console  →  {url}");
    println!("  bound to loopback only; press Ctrl-C to stop");

    if opts.open
        && let Err(e) = open_browser(&url)
    {
        eprintln!("  (could not open a browser automatically: {e})");
    }

    // Stop accepting *first*, then drain.
    //
    // `with_graceful_shutdown` keeps accepting until its future resolves, so
    // draining inside it left the door open: a Sets request arriving during
    // the wait spawned another detached rebuild, and one already inside a
    // handler could spawn after the idle check had passed. Axum then drains
    // the request and not the task it started. The signal resolves at once
    // now, and the waiting happens below, where nothing can create more.
    axum::serve(listener, app)
        .with_graceful_shutdown({
            let state = state.clone();
            async move {
                shutdown_signal().await;
                // Live streams have no natural end; this is theirs.
                state.begin_shutdown();
            }
        })
        .await
        .context("console server failed")?;

    drain_rebuilds(&state).await;

    println!("devbox console stopped.");
    Ok(())
}

/// Resolve when the process is asked to stop.
async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!(error = %e, "failed to install Ctrl-C handler");
    }
}

/// How long Ctrl-C waits for a rebuild to finish before giving up on it.
///
/// A `nixos-rebuild` on a cold store can genuinely take this long. The bound
/// exists so a wedged one cannot make Ctrl-C appear to do nothing, not because
/// a rebuild that is still running is a problem.
const REBUILD_DRAIN: std::time::Duration = std::time::Duration::from_secs(600);

/// Let any rebuild finish what it started, after the door is shut.
///
/// A rebuild runs detached, so the runtime used to drop it the instant `serve`
/// returned — and it can be dropped between replacing a box's generated files
/// and putting them back. What is lost is not the build but the cleanup: the
/// rollback, the posture restore, and the state write all live after the point
/// where cancellation lands, so the box is left on a selection nobody chose,
/// with its firewall down, and nothing on the host recording either.
/// `kill_on_drop` ends the child process and runs none of it.
async fn drain_rebuilds(state: &AppState) {
    if state.wait_for_rebuilds(std::time::Duration::ZERO).await {
        return;
    }
    println!("\ndevbox: a rebuild is in flight — waiting for it to finish or roll back.");
    println!("  Press Ctrl-C again to abandon it (the box may be left mid-selection).");

    // The second Ctrl-C has to be *waited on*, or that line is a lie: tokio's
    // handler stays installed for the life of the process, so the signal was
    // being consumed and discarded while the user sat out the whole timeout
    // believing they had cancelled.
    let abandoned = tokio::select! {
        drained = state.wait_for_rebuilds(REBUILD_DRAIN) => {
            if drained {
                return;
            }
            false
        }
        _ = shutdown_signal() => true,
    };

    if abandoned {
        eprintln!(
            "\ndevbox: abandoning the rebuild. The box may be left on a partial \
             selection with its egress posture not restored; `devbox sets apply` \
             or `devbox policy set` will put it right."
        );
    } else {
        eprintln!(
            "devbox: WARNING — a rebuild did not finish within {}s. The box may be \
             left on a partial selection with its egress posture not restored; \
             `devbox sets apply` or `devbox policy set` will put it right.",
            REBUILD_DRAIN.as_secs()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_url_includes_loopback_port_and_token() {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 7878));
        assert_eq!(
            console_url(&addr, "tok", "/"),
            "http://127.0.0.1:7878/?t=tok"
        );
    }

    #[test]
    fn console_url_lands_on_a_specific_box() {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 7878));
        assert_eq!(
            console_url(&addr, "tok", "/boxes/myapp"),
            "http://127.0.0.1:7878/boxes/myapp?t=tok"
        );
        assert_eq!(
            console_url(&addr, "tok", "/boxes/myapp?tab=terminal"),
            "http://127.0.0.1:7878/boxes/myapp?tab=terminal&t=tok"
        );
    }

    #[test]
    fn a_landing_path_that_is_not_a_path_falls_back_to_the_dashboard() {
        // Guards against a box name ever being spliced in as a full URL.
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 7878));
        assert_eq!(
            console_url(&addr, "tok", "https://evil.example/"),
            "http://127.0.0.1:7878/?t=tok"
        );
    }

    #[tokio::test]
    async fn binds_loopback_and_reports_the_actual_port() {
        // Port 0 lets the OS choose, which also proves we report the bound
        // port rather than the requested one.
        let (listener, addr) = bind_loopback(0, 1).await.unwrap();
        assert_eq!(addr.ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(addr.port(), 0);
        drop(listener);
    }

    #[tokio::test]
    async fn skips_a_port_that_is_already_taken() {
        let (held, addr) = bind_loopback(0, 1).await.unwrap();
        let taken = addr.port();

        let (next, next_addr) = bind_loopback(taken, PORT_SCAN).await.unwrap();
        assert_ne!(next_addr.port(), taken, "must not reuse the held port");
        assert!(next_addr.port() > taken);

        drop(next);
        drop(held);
    }

    #[tokio::test]
    async fn empty_scan_range_is_an_error_not_a_panic() {
        assert!(bind_loopback(7878, 0).await.is_err());
    }
}
