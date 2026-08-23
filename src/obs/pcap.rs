//! On-demand, real packet capture for one observed flow.
//!
//! Stored events intentionally contain metadata, not packet payloads. A pcap
//! export therefore asks the embedded guest agent to open a short AF_PACKET
//! capture for the selected five-tuple and streams the binary result back over
//! the runtime's authenticated exec channel. It never fabricates packets from
//! event rows.

use std::net::IpAddr;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::sandbox::SandboxManager;

pub const DEFAULT_DURATION: Duration = Duration::from_secs(10);
pub const DEFAULT_PACKETS: u16 = 256;
pub const MAX_DURATION: Duration = Duration::from_secs(60);
pub const MAX_PACKETS: u16 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowFilter {
    pub proto: String,
    pub saddr: Option<IpAddr>,
    pub sport: Option<u16>,
    pub daddr: IpAddr,
    pub dport: u16,
    pub duration: Duration,
    pub packets: u16,
}

impl FlowFilter {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.proto.as_str(), "tcp" | "udp") {
            bail!("flow protocol must be tcp or udp");
        }
        if self.dport == 0 {
            bail!("flow destination port must be non-zero");
        }
        if self.duration.is_zero() || self.duration > MAX_DURATION {
            bail!(
                "capture duration must be 1-{} seconds",
                MAX_DURATION.as_secs()
            );
        }
        if self.packets == 0 || self.packets > MAX_PACKETS {
            bail!("packet limit must be 1-{MAX_PACKETS}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Capture {
    pub bytes: Vec<u8>,
    pub packets: usize,
}

/// Capture one flow in a running box.
pub async fn capture(manager: &SandboxManager, name: &str, filter: &FlowFilter) -> Result<Capture> {
    filter.validate()?;

    // Hold the lifecycle claim from state read through the whole capture. A
    // concurrent `use`, stop, or destroy would otherwise redirect or kill the
    // guest command halfway through a binary response.
    let (claim, state) = manager.claim_and_read(name)?;
    crate::web::service::ensure_running_holding_claim(manager, name, &claim).await?;
    let runtime = manager.runtime_for_sandbox(&state)?;

    let mut direct = vec![
        "/usr/local/bin/devbox-obsd".to_string(),
        "-box-id".to_string(),
        name.to_string(),
        "-pcap".to_string(),
        "-pcap-proto".to_string(),
        filter.proto.clone(),
        "-pcap-daddr".to_string(),
        filter.daddr.to_string(),
        "-pcap-dport".to_string(),
        filter.dport.to_string(),
        "-pcap-duration".to_string(),
        format!("{}s", filter.duration.as_secs()),
        "-pcap-packets".to_string(),
        filter.packets.to_string(),
    ];
    if let Some(address) = filter.saddr {
        direct.push("-pcap-saddr".to_string());
        direct.push(address.to_string());
    }
    if let Some(port) = filter.sport {
        direct.push("-pcap-sport".to_string());
        direct.push(port.to_string());
    }

    // Decide privilege inside the guest without interpolating any flow value
    // into shell syntax. The pcap bytes remain stdout; diagnostics remain
    // stderr all the way back to the caller.
    let mut guest = vec![
        "sh".to_string(),
        "-c".to_string(),
        "if [ \"$(id -u)\" -eq 0 ]; then exec \"$@\"; else exec sudo -n \"$@\"; fi".to_string(),
        "devbox-obsd".to_string(),
    ];
    guest.append(&mut direct);
    let refs: Vec<&str> = guest.iter().map(String::as_str).collect();
    let argv = runtime.argv(name, &refs, false);
    let (program, args) = argv
        .split_first()
        .context("runtime returned an empty pcap command")?;

    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("start flow capture in box '{name}'"))?;

    // Read the streams under a size limit rather than with `output()`.
    //
    // Both are written by the guest, and `output()` buffers whatever it is
    // given until the process ends. The packet and duration limits are
    // arguments the guest is asked to honour, not bounds the host enforces —
    // so a compromised one could stream until the deadline and take the
    // console's memory with it.
    let mut stdout = child.stdout.take().context("pcap child has no stdout")?;
    let mut stderr = child.stderr.take().context("pcap child has no stderr")?;
    let collect = async {
        let mut bytes = Vec::new();
        let mut diagnostics = Vec::new();
        let out = read_capped(&mut stdout, &mut bytes, MAX_CAPTURE_BYTES);
        let err = read_capped(&mut stderr, &mut diagnostics, MAX_DIAGNOSTIC_BYTES);
        let (out, err) = tokio::join!(out, err);
        let overflowed = out?;
        err?;
        let status = child.wait().await?;
        Ok::<_, anyhow::Error>((status, bytes, diagnostics, overflowed))
    };
    let (status, bytes, diagnostics, overflowed) =
        tokio::time::timeout(filter.duration + Duration::from_secs(15), collect)
            .await
            .context("flow capture did not finish after its deadline")?
            .with_context(|| format!("read the flow capture from box '{name}'"))?;
    if overflowed {
        bail!(
            "flow capture from box '{name}' exceeded {} MiB and was cut off; \
             narrow it with a shorter duration or a smaller packet count",
            MAX_CAPTURE_BYTES / (1024 * 1024)
        );
    }
    if !status.success() {
        bail!(
            "flow capture failed in box '{name}': {}",
            String::from_utf8_lossy(&diagnostics).trim()
        );
    }
    let packets = validate(&bytes)?;
    if packets > usize::from(filter.packets) {
        // The ceiling is passed to the guest as an argument, which makes it a
        // request. Checking it here makes it a limit: a compromised agent
        // could otherwise answer a one-packet request with millions of
        // zero-length records and stay under the byte cap doing it.
        bail!(
            "flow capture from box '{name}' returned {packets} packets for a \
             request capped at {}",
            filter.packets
        );
    }
    Ok(Capture { bytes, packets })
}

/// Largest capture the console will hold in memory.
///
/// Comfortably above a full snaplen-limited capture at the packet ceiling, and
/// finite, which the guest's own honesty was not.
pub const MAX_CAPTURE_BYTES: usize = 64 * 1024 * 1024;

/// Largest diagnostic the console will keep from a failed capture.
pub const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;

/// Read a guest stream into `into`, refusing to grow past `limit`.
///
/// The stream is drained either way, so the guest is never blocked on a pipe
/// this side has stopped reading — it simply stops being remembered.
async fn read_capped<R>(mut reader: R, into: &mut Vec<u8>, limit: usize) -> Result<bool>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut chunk = [0u8; 64 * 1024];
    let mut overflowed = false;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(overflowed);
        }
        let room = limit.saturating_sub(into.len());
        if room < read {
            // Reported, not swallowed. A prefix that happens to end on a
            // packet boundary parses perfectly well, so a capture cut short
            // here would otherwise be handed over as a complete one.
            overflowed = true;
        }
        if room > 0 {
            into.extend_from_slice(&chunk[..read.min(room)]);
        }
    }
}

/// Validate the agent response before calling it a downloadable pcap.
pub fn validate(bytes: &[u8]) -> Result<usize> {
    if bytes.len() < 24 || bytes[..4] != [0xd4, 0xc3, 0xb2, 0xa1] {
        bail!("guest returned data that is not a little-endian Ethernet pcap");
    }
    if u16::from_le_bytes(bytes[4..6].try_into().unwrap()) != 2
        || u16::from_le_bytes(bytes[6..8].try_into().unwrap()) != 4
    {
        bail!("pcap uses an unsupported file format version");
    }
    let snaplen = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    if snaplen == 0 || snaplen > 65_535 {
        bail!("pcap declares an invalid snapshot length");
    }
    if u32::from_le_bytes(bytes[20..24].try_into().unwrap()) != 1 {
        bail!("pcap is not an Ethernet capture");
    }
    let mut offset = 24usize;
    let mut packets = 0usize;
    while offset < bytes.len() {
        if bytes.len() - offset < 16 {
            bail!("pcap ends inside a packet header");
        }
        let captured = u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap());
        let original = u32::from_le_bytes(bytes[offset + 12..offset + 16].try_into().unwrap());
        if captured > snaplen || captured > original {
            bail!("pcap packet {packets} has inconsistent lengths");
        }
        offset = offset
            .checked_add(16)
            .and_then(|value| value.checked_add(captured as usize))
            .context("pcap packet length overflow")?;
        if offset > bytes.len() {
            bail!("pcap ends inside packet {packets}");
        }
        packets += 1;
    }
    Ok(packets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_empty_and_populated_classic_pcaps() {
        let mut empty = vec![0xd4, 0xc3, 0xb2, 0xa1];
        empty.extend_from_slice(&[2, 0, 4, 0]);
        empty.extend_from_slice(&[0; 8]);
        empty.extend_from_slice(&65_535u32.to_le_bytes());
        empty.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(validate(&empty).unwrap(), 0);

        let mut one = empty;
        one.extend_from_slice(&[0; 8]);
        one.extend_from_slice(&3u32.to_le_bytes());
        one.extend_from_slice(&3u32.to_le_bytes());
        one.extend_from_slice(&[1, 2, 3]);
        assert_eq!(validate(&one).unwrap(), 1);
    }

    #[test]
    fn truncated_or_unbounded_captures_are_rejected() {
        assert!(validate(b"not pcap").is_err());
        let filter = FlowFilter {
            proto: "icmp".into(),
            saddr: None,
            sport: None,
            daddr: "127.0.0.1".parse().unwrap(),
            dport: 1,
            duration: Duration::from_secs(61),
            packets: 0,
        };
        assert!(filter.validate().is_err());

        let mut wrong_link = vec![0xd4, 0xc3, 0xb2, 0xa1, 2, 0, 4, 0];
        wrong_link.extend_from_slice(&[0; 8]);
        wrong_link.extend_from_slice(&65_535u32.to_le_bytes());
        wrong_link.extend_from_slice(&101u32.to_le_bytes());
        assert!(validate(&wrong_link).is_err());
    }
}
