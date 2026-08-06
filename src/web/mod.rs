//! Web console — the v4 primary interface.
//!
//! An [`axum`] server bound to loopback renders server-side HTML with
//! [`askama`], makes it live with htmx + Server-Sent Events, and shares its
//! control-plane logic with the CLI through [`service`]. All assets are
//! embedded in the binary (see [`assets`]), so the console needs no network
//! and no build step.

pub mod assets;
pub mod auth;
pub mod help;
pub mod routes;
pub mod server;
pub mod service;
pub mod sse;
pub mod state;
pub mod term;
pub mod watch;

pub use server::{WebOptions, serve};
