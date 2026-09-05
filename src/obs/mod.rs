//! Observability — the glass box (§7).
//!
//! The agent inside a box captures kernel-level activity and streams it here;
//! this module receives it, stores it, and answers questions about it.
//!
//! - [`event`] — the canonical schema (§11.1), mirrored in `agent/event`.
//! - [`store`] — the per-box SQLite store and its query API (§7.4).
//! - [`behavior`] — the run summary and behaviour diff (§7.6).
//! - [`collector`] — the socket listener and the agent handshake (§11.3).
//! - [`health`] — per-box capture health, so an empty view can say why.
//! - [`supervisor`] — one live collector per registered box.
//! - [`correlate`] — the join that turns a wall of events into a story (§7.2).
//! - [`run`] — a bounded execution with an identity, and its attribution (§4).
//! - [`redact`] — credentials out of an argv, on the way out.

pub mod behavior;
pub mod collector;
pub mod correlate;
pub mod daemon;
pub mod event;
pub mod health;
pub mod pcap;
pub mod redact;
pub mod run;
pub mod store;
pub mod supervisor;

pub use event::{Event, EventType};
pub use run::{Attribution, RunKind, RunRecord, RunStatus};
pub use store::{Query, Retention, Store};

/// Whether a box can connect to a host-owned Unix-domain socket through a
/// bind mount.
///
/// Only native Linux Docker shares the host kernel. Docker Desktop runs the
/// container in a Linux VM, where a socket inode exposed by virtiofs is data,
/// not a connectable endpoint. Those boxes use the runtime's authenticated
/// exec stdio transport just like Lima, Multipass and Incus.
pub(crate) fn uses_host_socket(runtime: &str) -> bool {
    cfg!(target_os = "linux") && runtime == "docker"
}

/// Whether this runtime may attach the embedded eBPF probes.
///
/// The programs deliberately trace the one box's whole kernel. That is
/// containment only for dedicated VM kernels; Docker shares its kernel with
/// other containers and must always use proc+packet capture.
pub(crate) fn uses_ebpf(runtime: &str) -> bool {
    crate::embedded::obsd_has_ebpf() && runtime != "docker"
}

#[cfg(test)]
mod transport_tests {
    #[test]
    fn only_native_linux_docker_uses_a_host_socket() {
        assert_eq!(super::uses_host_socket("docker"), cfg!(target_os = "linux"));
        for runtime in ["lima", "multipass", "incus"] {
            assert!(!super::uses_host_socket(runtime));
        }
    }

    #[test]
    fn shared_kernel_docker_never_uses_ebpf() {
        assert!(!super::uses_ebpf("docker"));
        for runtime in ["lima", "multipass", "incus"] {
            assert_eq!(super::uses_ebpf(runtime), crate::embedded::obsd_has_ebpf());
        }
    }
}
