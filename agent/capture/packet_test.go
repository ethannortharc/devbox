package capture

import (
	"encoding/binary"
	"net/netip"
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
