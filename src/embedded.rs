//! Artifacts which keep the release a single executable.

/// Linux observability agent for the same CPU architecture as this host
/// binary. `build.rs` either builds the portable proc+packet form for local
/// development or consumes the eBPF-enabled release artifact supplied by CI.
pub const OBSD: &[u8] = include_bytes!(env!("DEVBOX_EMBEDDED_OBSD"));

/// Linux zero-touch provisioning server for the guest architecture.
pub const ZTPD: &[u8] = include_bytes!(env!("DEVBOX_EMBEDDED_ZTPD"));

/// Whether the embedded agent includes the generated CO-RE loader.
pub fn obsd_has_ebpf() -> bool {
    env!("DEVBOX_EMBEDDED_OBSD_EBPF") == "1"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_observability_agent_is_really_embedded() {
        assert!(
            OBSD.len() > 1_000_000,
            "embedded agent is implausibly small"
        );
        assert_eq!(&OBSD[..4], b"\x7fELF", "guest agent must be a Linux ELF");
    }

    #[test]
    fn the_ztp_server_is_really_embedded() {
        assert!(
            ZTPD.len() > 1_000_000,
            "embedded ZTP server is implausibly small"
        );
        assert_eq!(&ZTPD[..4], b"\x7fELF", "ZTP server must be a Linux ELF");
    }
}
