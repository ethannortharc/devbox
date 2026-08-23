package capture

import (
	"bytes"
	"encoding/binary"
	"testing"
	"time"
)

func TestPCAPWriterProducesAStandardEthernetCapture(t *testing.T) {
	var out bytes.Buffer
	if err := writePCAPHeader(&out); err != nil {
		t.Fatal(err)
	}
	frame := []byte{0, 1, 2, 3, 4, 5}
	if err := writePCAPPacket(&out, time.Unix(7, 123000), frame); err != nil {
		t.Fatal(err)
	}
	raw := out.Bytes()
	if got := binary.LittleEndian.Uint32(raw[:4]); got != 0xa1b2c3d4 {
		t.Fatalf("magic = %#x", got)
	}
	if got := binary.LittleEndian.Uint32(raw[20:24]); got != 1 {
		t.Fatalf("link type = %d, want Ethernet", got)
	}
	if got := binary.LittleEndian.Uint32(raw[32:36]); got != uint32(len(frame)) {
		t.Fatalf("captured length = %d", got)
	}
	if !bytes.Equal(raw[40:], frame) {
		t.Fatalf("packet bytes = %v", raw[40:])
	}
}

func TestPCAPFilterMatchesBothDirectionsAndNothingElse(t *testing.T) {
	filter := PCAPFilter{
		Proto: "tcp", SAddr: "10.0.0.2", SPort: 42000,
		DAddr: "203.0.113.9", DPort: 443,
		Duration: time.Second, MaxPackets: 1,
	}
	out := packetView{proto: "tcp", saddr: "10.0.0.2", sport: 42000, daddr: "203.0.113.9", dport: 443}
	in := packetView{proto: "tcp", saddr: "203.0.113.9", sport: 443, daddr: "10.0.0.2", dport: 42000}
	wrong := packetView{proto: "tcp", saddr: "10.0.0.2", sport: 42001, daddr: "203.0.113.9", dport: 443}
	if !filter.matches(out) || !filter.matches(in) || filter.matches(wrong) {
		t.Fatalf("bidirectional flow filter did not preserve the exact five-tuple")
	}
}
