//! Artifacts which keep the release a single executable.

/// Linux observability agent for the same CPU architecture as this host
/// binary. `build.rs` either builds the portable proc+packet form for local
/// development or consumes the eBPF-enabled release artifact supplied by CI.
pub const OBSD: &[u8] = include_bytes!(env!("DEVBOX_EMBEDDED_OBSD"));

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
}
