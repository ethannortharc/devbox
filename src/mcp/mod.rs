//! MCP sandboxing — §7 of the v5 design.
//!
//! An MCP server is an ordinary program that speaks JSON-RPC over stdin and
//! stdout. Agents launch it themselves, which means it runs on the host with
//! the agent's privileges: the one process in an "isolated" workflow that is
//! not isolated at all.
//!
//! Devbox puts it in a box without the agent noticing. [`registry`] records
//! `[mcp.<name>]` in `devbox.toml`; [`shim`] is the host-side process the agent
//! actually launches, which starts the registered command *inside* the box and
//! carries the byte stream between the two. To the agent it is a plain stdio
//! MCP server; to the box it is one more observed process under a posture.
//!
//! Two properties are load-bearing and both are tested:
//!
//! - **Byte transparency.** JSON-RPC framing is the client's business, not
//!   ours. The shim copies raw buffers and never looks inside them, so a
//!   payload with CRLF, NUL, or invalid UTF-8 arrives unchanged.
//! - **No orphans.** The guest process must not outlive the shim. `limactl
//!   shell` is `ssh` underneath and a non-tty ssh channel delivers no SIGHUP,
//!   so a guest process that ignores stdin survives its transport by default.
//!   [`shim::wrap_guest_command`] and [`shim::reaper_script`] are the answer.

pub mod registry;
pub mod rpc;
pub mod shim;
