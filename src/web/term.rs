//! Browser terminal — xterm.js ↔ WebSocket ↔ pty.
//!
//! This is the only bidirectional view in the console, and the only reason it
//! uses a WebSocket rather than SSE. The server owns a real pty, runs the
//! runtime's interactive argv inside it (see [`crate::runtime::Runtime`]),
//! and shuttles bytes:
//!
//! ```text
//!   xterm.js ──(binary frames: keystrokes)──▶ pty master ──▶ shell in box
//!   xterm.js ◀─(binary frames: output)────── pty master ◀── shell in box
//!   xterm.js ──(text frame: {"resize":[cols,rows]})──▶ TIOCSWINSZ
//! ```
//!
//! Input and output are binary frames so no byte is ever mangled by UTF-8
//! validation — a terminal stream is not text, it is bytes with escape
//! sequences, and half a UTF-8 sequence can legitimately straddle a read.

use std::io::{Read, Write};
use std::sync::Mutex;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use serde::Deserialize;
use tokio::sync::mpsc;

/// Terminal size, in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Default for Size {
    fn default() -> Self {
        // A sane 80x24 until the browser reports its real geometry, which it
        // does immediately on connect.
        Self { cols: 80, rows: 24 }
    }
}

impl Size {
    /// Clamp to something a pty will accept.
    ///
    /// A browser that reports 0 columns (a hidden tab, a detached canvas) would
    /// otherwise hand `TIOCSWINSZ` a zero and confuse every full-screen program
    /// in the box.
    pub fn sanitized(self) -> Self {
        Self {
            cols: self.cols.clamp(2, 1000),
            rows: self.rows.clamp(2, 1000),
        }
    }

    fn to_pty_size(self) -> PtySize {
        let s = self.sanitized();
        PtySize {
            rows: s.rows,
            cols: s.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// A control message from the browser. Anything not understood is ignored
/// rather than closing the session.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMessage {
    Resize(Size),
}

/// Parse a control frame. Returns `None` for anything unrecognized.
pub fn parse_control(text: &str) -> Option<ClientMessage> {
    serde_json::from_str(text).ok()
}

/// How much pty output to read per chunk.
const READ_CHUNK: usize = 8 * 1024;

/// How many output chunks may queue before the reader blocks.
///
/// Bounded on purpose: a `yes` loop in the box must apply backpressure to the
/// pty rather than grow an unbounded queue in the server.
const OUTPUT_QUEUE: usize = 64;

/// Run a terminal session until either side hangs up.
///
/// `argv` is the host-side command that enters the box, from
/// [`crate::runtime::Runtime::interactive_argv`].
pub async fn run_session(socket: WebSocket, argv: Vec<String>, initial: Size) -> Result<()> {
    let (mut child, pty) = spawn_pty(&argv, initial)?;

    let mut reader = pty
        .master
        .try_clone_reader()
        .context("failed to clone pty reader")?;
    let writer = pty
        .master
        .take_writer()
        .context("failed to take pty writer")?;
    // `MasterPty` is `Send` but not `Sync`, and the socket task is a spawned
    // future that must be `Send` as a whole — so the handle lives behind a
    // mutex. It is only ever locked for a `resize` ioctl, never across an
    // await, so there is no contention worth measuring.
    let master = Mutex::new(pty.master);

    // Reading a pty is a blocking syscall, so it lives on a blocking thread and
    // hands chunks over a bounded channel.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(OUTPUT_QUEUE);
    let reader_task = tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buf) {
                // EOF: the shell exited.
                Ok(0) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break; // browser went away
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "pty read ended");
                    break;
                }
            }
        }
    });

    // Writing is also blocking; serialize it through a channel so the socket
    // task never blocks the runtime.
    let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(OUTPUT_QUEUE);
    let writer_task = tokio::task::spawn_blocking(move || {
        let mut writer = writer;
        while let Some(bytes) = in_rx.blocking_recv() {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    let (mut ws_tx, mut ws_rx) = {
        use futures::StreamExt;
        socket.split()
    };

    let pump_out = async {
        use futures::SinkExt;
        while let Some(chunk) = out_rx.recv().await {
            if ws_tx.send(Message::Binary(chunk.into())).await.is_err() {
                break;
            }
        }
        // Shell exited: closing the socket tells xterm.js the session is over.
        let _ = ws_tx.close().await;
    };

    let pump_in = async {
        use futures::StreamExt;
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Binary(bytes) => {
                    if in_tx.send(bytes.to_vec()).await.is_err() {
                        break;
                    }
                }
                Message::Text(text) => {
                    if let Some(ClientMessage::Resize(size)) = parse_control(text.as_str())
                        && let Ok(master) = master.lock()
                        && let Err(e) = master.resize(size.to_pty_size())
                    {
                        tracing::debug!(error = %e, "pty resize failed");
                    }
                }
                Message::Close(_) => break,
                // Ping/Pong are handled by axum.
                _ => {}
            }
        }
    };

    tokio::select! {
        _ = pump_out => {}
        _ = pump_in => {}
    }

    // Ordered teardown. Killing the child closes the pty slave, so the reader
    // thread sees EOF; dropping the input sender ends the writer thread. Both
    // are blocking threads, so they are detached rather than awaited — a
    // `spawn_blocking` task cannot be cancelled mid-syscall, and holding the
    // request open to wait for one would be worse than letting it drain.
    let _ = child.kill();
    let _ = child.wait();
    drop(in_tx);
    drop(reader_task);
    drop(writer_task);
    Ok(())
}

/// A spawned pty. The child is returned separately so the caller can reap it.
struct Pty {
    master: Box<dyn portable_pty::MasterPty + Send>,
}

fn spawn_pty(argv: &[String], size: Size) -> Result<(Box<dyn portable_pty::Child + Send>, Pty)> {
    let (program, args) = argv
        .split_first()
        .context("interactive argv must not be empty")?;

    let pair = NativePtySystem::default()
        .openpty(size.to_pty_size())
        .context("failed to open a pty")?;

    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    // xterm.js speaks xterm-256color; telling the box otherwise produces a
    // shell that draws no colours and mis-handles keys.
    cmd.env("TERM", "xterm-256color");

    let child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("failed to launch `{program}` for the browser terminal"))?;

    // Dropping the slave lets the pty report EOF once the child exits.
    drop(pair.slave);

    Ok((
        child,
        Pty {
            master: pair.master,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_size_is_a_classic_terminal() {
        assert_eq!(Size::default(), Size { cols: 80, rows: 24 });
    }

    #[test]
    fn zero_and_absurd_sizes_are_clamped() {
        assert_eq!(
            Size { cols: 0, rows: 0 }.sanitized(),
            Size { cols: 2, rows: 2 }
        );
        assert_eq!(
            Size {
                cols: 65535,
                rows: 65535
            }
            .sanitized(),
            Size {
                cols: 1000,
                rows: 1000
            }
        );
        let ok = Size {
            cols: 120,
            rows: 40,
        };
        assert_eq!(ok.sanitized(), ok);
    }

    #[test]
    fn parses_a_resize_control_frame() {
        let msg = parse_control(r#"{"type":"resize","cols":120,"rows":40}"#);
        match msg {
            Some(ClientMessage::Resize(size)) => {
                assert_eq!(
                    size,
                    Size {
                        cols: 120,
                        rows: 40
                    }
                )
            }
            other => panic!("expected a resize, got {other:?}"),
        }
    }

    #[test]
    fn junk_control_frames_are_ignored_not_fatal() {
        assert!(parse_control("not json").is_none());
        assert!(parse_control(r#"{"type":"launch-missiles"}"#).is_none());
        assert!(parse_control(r#"{"type":"resize"}"#).is_none());
    }

    #[test]
    fn spawning_needs_a_program() {
        assert!(spawn_pty(&[], Size::default()).is_err());
    }

    #[test]
    fn pty_runs_a_command_and_reports_its_output() {
        let (mut child, pty) =
            spawn_pty(&["echo".into(), "hello-from-pty".into()], Size::default())
                .expect("spawn echo");

        let mut reader = pty.master.try_clone_reader().unwrap();
        let mut out = String::new();
        let mut buf = [0u8; 512];
        // A pty echoes until the child exits and the slave closes.
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.push_str(&String::from_utf8_lossy(&buf[..n]));
            if out.contains("hello-from-pty") {
                break;
            }
        }
        let _ = child.wait();

        assert!(out.contains("hello-from-pty"), "pty output was: {out:?}");
    }
}
