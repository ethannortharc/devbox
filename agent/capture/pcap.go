package capture

import (
	"encoding/binary"
	"fmt"
	"io"
	"net/netip"
	"time"
)

// PCAPFilter selects one bidirectional transport flow for an on-demand
// capture. Empty source fields are wildcards; the remote destination and port
// are required by the CLI and web control planes.
type PCAPFilter struct {
	Proto        string
	SAddr, DAddr string
	SPort, DPort uint16
	Duration     time.Duration
	MaxPackets   int
}

func (f PCAPFilter) validate() error {
	if f.Proto != "tcp" && f.Proto != "udp" {
		return fmt.Errorf("pcap protocol must be tcp or udp, got %q", f.Proto)
	}
	if f.DAddr == "" || f.DPort == 0 {
		return fmt.Errorf("pcap destination address and port are required")
	}
	if _, err := netip.ParseAddr(f.DAddr); err != nil {
		return fmt.Errorf("invalid pcap destination address %q: %w", f.DAddr, err)
	}
	if f.SAddr != "" {
		if _, err := netip.ParseAddr(f.SAddr); err != nil {
			return fmt.Errorf("invalid pcap source address %q: %w", f.SAddr, err)
		}
	}
	if f.Duration <= 0 || f.Duration > time.Minute {
		return fmt.Errorf("pcap duration must be greater than zero and at most one minute")
	}
	if f.MaxPackets < 1 || f.MaxPackets > 4096 {
		return fmt.Errorf("pcap packet limit must be between 1 and 4096")
	}
	return nil
}

func (f PCAPFilter) matches(view packetView) bool {
	if view.proto != f.Proto {
		return false
	}
	direct := view.daddr == f.DAddr && view.dport == f.DPort
	reverse := view.saddr == f.DAddr && view.sport == f.DPort
	if f.SAddr != "" {
		direct = direct && view.saddr == f.SAddr
		reverse = reverse && view.daddr == f.SAddr
	}
	if f.SPort != 0 {
		direct = direct && view.sport == f.SPort
		reverse = reverse && view.dport == f.SPort
	}
	return direct || reverse
}

func writePCAPHeader(out io.Writer) error {
	// Classic pcap, little endian, Ethernet link type. Wireshark and tcpdump
	// both read it without optional libraries or pcapng metadata support.
	fields := []any{
		uint32(0xa1b2c3d4),
		uint16(2), uint16(4),
		int32(0), uint32(0),
		uint32(65535), uint32(1),
	}
	for _, field := range fields {
		if err := binary.Write(out, binary.LittleEndian, field); err != nil {
			return fmt.Errorf("write pcap header: %w", err)
		}
	}
	return nil
}

func writePCAPPacket(out io.Writer, captured time.Time, frame []byte) error {
	length := len(frame)
	if length > 65535 {
		length = 65535
	}
	for _, field := range []uint32{
		uint32(captured.Unix()),
		uint32(captured.Nanosecond() / 1000),
		uint32(length),
		uint32(len(frame)),
	} {
		if err := binary.Write(out, binary.LittleEndian, field); err != nil {
			return fmt.Errorf("write pcap packet header: %w", err)
		}
	}
	if _, err := out.Write(frame[:length]); err != nil {
		return fmt.Errorf("write pcap packet: %w", err)
	}
	return nil
}
