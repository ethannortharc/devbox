package capture

import (
	"bytes"
	"encoding/binary"
	"net/netip"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

func dnsQuery(name string) []byte {
	message := make([]byte, 12)
	binary.BigEndian.PutUint16(message[0:2], 0x1234)
	binary.BigEndian.PutUint16(message[2:4], 0x0100)
	binary.BigEndian.PutUint16(message[4:6], 1)
	for _, label := range strings.Split(name, ".") {
		message = append(message, byte(len(label)))
		message = append(message, label...)
	}
	message = append(message, 0, 0, 1, 0, 1)
	return message
}

func ipv4UDPFrame(srcPort, dstPort uint16, payload []byte) []byte {
	return ipv4UDPFrameFrom(
		[4]byte{10, 0, 0, 2},
		[4]byte{10, 0, 0, 53},
		srcPort,
		dstPort,
		payload,
	)
}

func ipv4UDPFrameFrom(src, dst [4]byte, srcPort, dstPort uint16, payload []byte) []byte {
	frame := make([]byte, 14+20+8+len(payload))
	binary.BigEndian.PutUint16(frame[12:14], 0x0800)
	ip := frame[14:]
	ip[0] = 0x45
	binary.BigEndian.PutUint16(ip[2:4], uint16(20+8+len(payload)))
	ip[8] = 64
	ip[9] = 17
	copy(ip[12:16], src[:])
	copy(ip[16:20], dst[:])
	udp := ip[20:]
	binary.BigEndian.PutUint16(udp[0:2], srcPort)
	binary.BigEndian.PutUint16(udp[2:4], dstPort)
	binary.BigEndian.PutUint16(udp[4:6], uint16(8+len(payload)))
	copy(udp[8:], payload)
	return frame
}

func dnsAResponse(name string, address [4]byte) []byte {
	message := dnsQuery(name)
	message[2] |= 0x80
	binary.BigEndian.PutUint16(message[6:8], 1)
	message = append(message,
		0xc0, 0x0c, // answer name points at the question
		0x00, 0x01, // A
		0x00, 0x01, // IN
		0x00, 0x00, 0x00, 0x3c, // TTL
		0x00, 0x04,
	)
	return append(message, address[:]...)
}

func TestPacketSourceProducesARealDNSDomain(t *testing.T) {
	t.Parallel()

	boot := time.Unix(100, 0)
	source := &Packet{
		BoxID: "mybox",
		Boot:  boot,
		Resolvers: map[netip.Addr]struct{}{
			netip.MustParseAddr("10.0.0.53"): {},
		},
	}
	got := source.decodePacket(
		ipv4UDPFrame(41000, 53, dnsQuery("api.anthropic.com")),
		boot.Add(5*time.Second),
	)
	if got == nil {
		t.Fatal("DNS packet produced no event")
	}
	if got.Type != event.TypeDNS || got.Net == nil {
		t.Fatalf("event = %+v, want DNS", got)
	}
	if got.Net.QName != "api.anthropic.com" || got.Net.QType != "A" {
		t.Errorf("DNS metadata = %+v", got.Net)
	}
	if got.PID != UnattributedPID {
		t.Errorf("packet without process correlation got pid %d", got.PID)
	}
	if err := got.Validate(); err != nil {
		t.Errorf("packet source emitted an invalid event: %v", err)
	}
}

func TestPacketSourceAcceptsOnlyTheMatchingResolverAnswer(t *testing.T) {
	t.Parallel()

	boot := time.Unix(100, 0)
	source := &Packet{
		BoxID: "mybox",
		Boot:  boot,
		Resolvers: map[netip.Addr]struct{}{
			netip.MustParseAddr("10.0.0.53"): {},
		},
	}
	queryAt := boot.Add(time.Second)
	if query := source.decodePacket(
		ipv4UDPFrame(41000, 53, dnsQuery("github.com")),
		queryAt,
	); query == nil || query.Net.Response {
		t.Fatalf("outbound query was not recorded: %+v", query)
	}
	answer := source.decodePacket(
		ipv4UDPFrameFrom(
			[4]byte{10, 0, 0, 53},
			[4]byte{10, 0, 0, 2},
			53,
			41000,
			dnsAResponse("github.com", [4]byte{140, 82, 112, 4}),
		),
		queryAt.Add(time.Millisecond),
	)
	if answer == nil || !answer.Net.Response {
		t.Fatalf("matching resolver answer was dropped: %+v", answer)
	}
	if len(answer.Net.Answers) != 1 || answer.Net.Answers[0] != "140.82.112.4" {
		t.Errorf("answers = %v", answer.Net.Answers)
	}
}

func TestPacketSourceRejectsAnUnsolicitedDNSAnswer(t *testing.T) {
	t.Parallel()

	boot := time.Unix(100, 0)
	source := &Packet{
		BoxID: "mybox",
		Boot:  boot,
		Resolvers: map[netip.Addr]struct{}{
			netip.MustParseAddr("10.0.0.53"): {},
		},
	}
	answer := dnsAResponse("github.com", [4]byte{203, 0, 113, 7})
	// Even a syntactically valid response from the configured resolver is not
	// trusted unless this tap observed the matching query first.
	if got := source.decodePacket(ipv4UDPFrameFrom(
		[4]byte{10, 0, 0, 53},
		[4]byte{10, 0, 0, 2},
		53,
		41000,
		answer,
	), boot.Add(time.Second)); got != nil {
		t.Fatalf("unsolicited DNS answer became an event: %+v", got)
	}
}

func TestPacketParserRejectsTruncationWithoutPanicking(t *testing.T) {
	t.Parallel()

	source := &Packet{BoxID: "mybox", Boot: time.Now()}
	for size := 0; size < 42; size++ {
		if got := source.decodePacket(make([]byte, size), time.Now()); got != nil {
			t.Errorf("%d zero bytes unexpectedly decoded as %+v", size, got)
		}
	}
}

// ── TLS ClientHello reassembly ───────────────────────────
//
// The fixtures live in the decoder's testdata because the decoder owns their
// shape; see agent/decode/testdata/README.md for how they were captured and why
// they are 1521 bytes.

const (
	// clientHelloMSS is the segment size observed on devtest, where one
	// `curl https://example.com` split its ClientHello 1448 + 112.
	clientHelloMSS = 1448
)

// firstDataSeq is picked so the ClientHello straddles the 32-bit sequence wrap:
// 0xFFFFFC00 + 1448 overflows, and a reassembler that follows the sequence
// number has to follow it round. A var, not a const, because that sum is not
// representable as an untyped constant at all.
var firstDataSeq = uint32(0xFFFFFC00)

func loadClientHello(t *testing.T, name string) []byte {
	t.Helper()
	record, err := os.ReadFile(filepath.Join("..", "decode", "testdata", name))
	if err != nil {
		t.Fatalf("read fixture: %v", err)
	}
	if len(record) <= clientHelloMSS {
		t.Fatalf("fixture %s fits one segment and so tests nothing", name)
	}
	return record
}

func ipv4TCPFrame(srcPort uint16, seq uint32, payload []byte) []byte {
	frame := make([]byte, 14+20+20+len(payload))
	binary.BigEndian.PutUint16(frame[12:14], 0x0800)
	ip := frame[14:]
	ip[0] = 0x45
	binary.BigEndian.PutUint16(ip[2:4], uint16(20+20+len(payload)))
	ip[8] = 64
	ip[9] = 6 // TCP
	copy(ip[12:16], []byte{10, 0, 0, 2})
	copy(ip[16:20], []byte{93, 184, 216, 34})
	tcp := ip[20:]
	binary.BigEndian.PutUint16(tcp[0:2], srcPort)
	binary.BigEndian.PutUint16(tcp[2:4], 443)
	binary.BigEndian.PutUint32(tcp[4:8], seq)
	tcp[12] = 5 << 4 // data offset: 20 bytes, no options
	tcp[13] = 0x18   // PSH|ACK
	binary.BigEndian.PutUint16(tcp[14:16], 0xFFFF)
	copy(tcp[20:], payload)
	return frame
}

// TestPacketSourceReadsSNIFromTheFirstSegment is the common case and the one
// that was broken: server_name sits at byte 108, the segment is 1448 bytes, and
// the record claims 1521. The name is answerable from this segment alone, so
// nothing should be held afterwards.
func TestPacketSourceReadsSNIFromTheFirstSegment(t *testing.T) {
	t.Parallel()

	record := loadClientHello(t, "clienthello_pq.bin")
	boot := time.Unix(100, 0)
	source := &Packet{BoxID: "mybox", Boot: boot}

	got := source.decodePacket(
		ipv4TCPFrame(41000, firstDataSeq, record[:clientHelloMSS]),
		boot.Add(time.Second),
	)
	if got == nil {
		t.Fatal("the first segment of a post-quantum ClientHello produced no event")
	}
	if got.Type != event.TypeTLS || got.Net == nil {
		t.Fatalf("event = %+v, want TLS", got)
	}
	if got.Net.SNI != "example.com" {
		t.Errorf("SNI = %q, want example.com", got.Net.SNI)
	}
	if got.Net.ALPN != "h2" {
		t.Errorf("ALPN = %q, want h2", got.Net.ALPN)
	}
	if err := got.Validate(); err != nil {
		t.Errorf("packet source emitted an invalid event: %v", err)
	}
	if len(source.tlsFlows) != 0 {
		t.Errorf("a flow answered from one segment left %d buffered", len(source.tlsFlows))
	}
}

// TestPacketSourceJoinsASplitClientHello is the reassembly path: server_name
// lands at bytes 1501..1521, past the segment boundary, so no single segment
// carries it.
func TestPacketSourceJoinsASplitClientHello(t *testing.T) {
	t.Parallel()

	record := loadClientHello(t, "clienthello_pq_sni_last.bin")
	boot := time.Unix(100, 0)
	source := &Packet{BoxID: "mybox", Boot: boot}
	at := boot.Add(time.Second)

	first := source.decodePacket(
		ipv4TCPFrame(41000, firstDataSeq, record[:clientHelloMSS]),
		at,
	)
	if first != nil {
		t.Fatalf("a segment with no server_name in it produced %+v", first)
	}
	if len(source.tlsFlows) != 1 {
		t.Fatalf("the incomplete ClientHello was not held: %d flows", len(source.tlsFlows))
	}

	second := source.decodePacket(
		ipv4TCPFrame(41000, firstDataSeq+clientHelloMSS, record[clientHelloMSS:]),
		at.Add(time.Millisecond),
	)
	if second == nil {
		t.Fatal("the segment completing the ClientHello produced no event")
	}
	if second.Net.SNI != "example.com" {
		t.Errorf("SNI = %q, want example.com", second.Net.SNI)
	}
	if len(source.tlsFlows) != 0 {
		t.Errorf("a decided flow left %d buffered", len(source.tlsFlows))
	}
}

// TestPacketSourceRefusesToSpliceARetransmission is why the join follows the
// sequence number instead of arrival order. Appending a duplicate of the first
// segment would put 1448 bytes of nonsense where the rest of the ClientHello
// belongs, and the parser would read a name out of it.
func TestPacketSourceRefusesToSpliceARetransmission(t *testing.T) {
	t.Parallel()

	record := loadClientHello(t, "clienthello_pq_sni_last.bin")
	boot := time.Unix(100, 0)
	source := &Packet{BoxID: "mybox", Boot: boot}
	at := boot.Add(time.Second)

	first := ipv4TCPFrame(41000, firstDataSeq, record[:clientHelloMSS])
	if got := source.decodePacket(first, at); got != nil {
		t.Fatalf("first segment produced %+v", got)
	}
	if got := source.decodePacket(first, at.Add(time.Millisecond)); got != nil {
		t.Fatalf("a retransmitted first segment produced %+v", got)
	}
	held := source.tlsFlows[tlsFlowKey{saddr: "10.0.0.2", daddr: "93.184.216.34", sport: 41000, dport: 443}]
	if held == nil {
		t.Fatal("the retransmission dropped the flow entirely")
	}
	if len(held.data) != clientHelloMSS {
		t.Errorf("held %d bytes after a duplicate segment, want %d — the duplicate was spliced in",
			len(held.data), clientHelloMSS)
	}

	got := source.decodePacket(
		ipv4TCPFrame(41000, firstDataSeq+clientHelloMSS, record[clientHelloMSS:]),
		at.Add(2*time.Millisecond),
	)
	if got == nil || got.Net.SNI != "example.com" {
		t.Fatalf("after a retransmission the flow decoded to %+v", got)
	}
}

// TestPacketSourceDropsASegmentThatDoesNotContinueTheFlow covers the gap: a
// segment arriving out of order cannot be joined, and guessing is worse than
// missing an SNI.
func TestPacketSourceDropsASegmentThatDoesNotContinueTheFlow(t *testing.T) {
	t.Parallel()

	record := loadClientHello(t, "clienthello_pq_sni_last.bin")
	boot := time.Unix(100, 0)
	source := &Packet{BoxID: "mybox", Boot: boot}
	at := boot.Add(time.Second)

	if got := source.decodePacket(ipv4TCPFrame(41000, firstDataSeq, record[:clientHelloMSS]), at); got != nil {
		t.Fatalf("first segment produced %+v", got)
	}
	// One byte further on than the flow is owed.
	got := source.decodePacket(
		ipv4TCPFrame(41000, firstDataSeq+clientHelloMSS+1, record[clientHelloMSS:]),
		at.Add(time.Millisecond),
	)
	if got != nil {
		t.Fatalf("a segment with a hole before it produced %+v", got)
	}
}

// TestPacketSourceForgetsAStaleClientHello: a flow that stalls mid-handshake
// must not hold its prefix indefinitely.
func TestPacketSourceForgetsAStaleClientHello(t *testing.T) {
	t.Parallel()

	record := loadClientHello(t, "clienthello_pq_sni_last.bin")
	boot := time.Unix(100, 0)
	source := &Packet{BoxID: "mybox", Boot: boot}
	at := boot.Add(time.Second)

	if got := source.decodePacket(ipv4TCPFrame(41000, firstDataSeq, record[:clientHelloMSS]), at); got != nil {
		t.Fatalf("first segment produced %+v", got)
	}
	late := at.Add(tlsAssemblyTTL + time.Second)
	got := source.decodePacket(
		ipv4TCPFrame(41000, firstDataSeq+clientHelloMSS, record[clientHelloMSS:]),
		late,
	)
	if got != nil {
		t.Fatalf("a segment %s late completed a ClientHello: %+v", tlsAssemblyTTL, got)
	}
	if len(source.tlsFlows) != 0 {
		t.Errorf("the expired prefix survived: %d flows", len(source.tlsFlows))
	}
}

// TestPacketSourceDiscardsAnEndlessClientHello is the hostile shape: a record
// header claiming 65535 bytes, 100 supplied, and a sender happy to dribble more
// forever. The segment cap must end it, with no event and nothing retained.
func TestPacketSourceDiscardsAnEndlessClientHello(t *testing.T) {
	t.Parallel()

	record := loadClientHello(t, "clienthello_pq_sni_last.bin")
	hostile := append([]byte(nil), record[:100]...)
	hostile[3], hostile[4] = 0xFF, 0xFF // the record claims 65535 bytes

	boot := time.Unix(100, 0)
	source := &Packet{BoxID: "mybox", Boot: boot}
	at := boot.Add(time.Second)

	// Filler that is structurally hopeless but never decidable: 0x17 is
	// application_data, so a segment of it can never begin a handshake record,
	// and inside the assembly it only ever declares lengths larger than what is
	// present. Exactly the input a cap, rather than the parser, has to stop.
	filler := bytes.Repeat([]byte{0x17}, 100)

	seq := firstDataSeq
	for segment := 0; segment < maxTLSSegments+2; segment++ {
		payload := filler
		if segment == 0 {
			payload = hostile
		}
		got := source.decodePacket(ipv4TCPFrame(41000, seq, payload), at)
		if got != nil {
			t.Fatalf("segment %d of an endless ClientHello produced %+v", segment, got)
		}
		for key, held := range source.tlsFlows {
			if held.segments > maxTLSSegments {
				t.Fatalf("%v holds %d segments, cap is %d", key, held.segments, maxTLSSegments)
			}
			if len(held.data) > maxTLSAssembly {
				t.Fatalf("%v holds %d bytes, cap is %d", key, len(held.data), maxTLSAssembly)
			}
		}
		seq += uint32(len(payload))
		at = at.Add(time.Millisecond)
	}
	if len(source.tlsFlows) != 0 {
		t.Errorf("a stream that never completes left %d flows held", len(source.tlsFlows))
	}
}

// TestPacketSourceBoundsTheNumberOfHeldFlows: the table is per-flow, so the
// cheapest attack is many flows rather than one large one.
func TestPacketSourceBoundsTheNumberOfHeldFlows(t *testing.T) {
	t.Parallel()

	record := loadClientHello(t, "clienthello_pq_sni_last.bin")
	prefix := record[:100]

	boot := time.Unix(100, 0)
	source := &Packet{BoxID: "mybox", Boot: boot}
	at := boot.Add(time.Second)

	for i := 0; i < maxTLSFlows*2; i++ {
		port := uint16(1024 + i%60000)
		if got := source.decodePacket(ipv4TCPFrame(port, firstDataSeq, prefix), at); got != nil {
			t.Fatalf("flow %d produced %+v", i, got)
		}
		if len(source.tlsFlows) > maxTLSFlows {
			t.Fatalf("after %d flows the table holds %d, cap is %d",
				i+1, len(source.tlsFlows), maxTLSFlows)
		}
	}
	if len(source.tlsFlows) != maxTLSFlows {
		t.Errorf("table holds %d flows, want it filled to %d", len(source.tlsFlows), maxTLSFlows)
	}
}

// TestPacketPayloadStopsAtTheDeclaredIPLength: Ethernet pads short frames to 60
// bytes. Taking the TCP payload to the end of the frame swept those pad bytes
// in — invisible while every segment was judged alone, but they would be joined
// into the middle of a ClientHello.
func TestPacketPayloadStopsAtTheDeclaredIPLength(t *testing.T) {
	t.Parallel()

	payload := []byte{0x16, 0x03, 0x01}
	frame := ipv4TCPFrame(41000, firstDataSeq, payload)
	padded := append(append([]byte(nil), frame...), make([]byte, 60-len(frame))...)

	view, ok := networkPayload(padded)
	if !ok {
		t.Fatal("a padded frame was rejected outright")
	}
	if len(view.payload) != len(payload) {
		t.Errorf("payload is %d bytes, want %d — Ethernet padding was included",
			len(view.payload), len(payload))
	}
	if view.seq != firstDataSeq {
		t.Errorf("seq = %#x, want %#x", view.seq, firstDataSeq)
	}
}
