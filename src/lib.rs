//! devbox — isolated developer boxes for AI coding agents and humans.
//!
//! The crate is built as a library plus a thin binary so that the control
//! plane, the web console, the observability collector, and the lab
//! orchestrator can all be driven directly from integration tests in
//! `tests/` — not only through the CLI surface.
//!
//! Layers, outermost first:
//!
//! - [`cli`] — command surface (clap), one module per command.
//! - [`web`] — the v4 web console: axum server, askama templates, SSE.
//! - [`sandbox`] — box lifecycle, state, config, OverlayFS diff/commit.
//! - [`nix`] — Nix set composition and rebuilds inside a box.
//! - [`runtime`] — the Incus/Lima/Multipass/Docker abstraction.
//! - [`tools`] — language/tool detection and the tool registry.

pub mod cli;
pub mod nix;
pub mod runtime;
pub mod sandbox;
pub mod tools;
pub mod tui;
pub mod web;
