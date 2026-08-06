package decode

import (
	"encoding/binary"
	"errors"
	"fmt"
	"net/netip"
	"strings"
)

// DNS and TLS parsing.
//
// §13 names gopacket for this. Both formats are parsed here by hand instead
// (ADR-0016): the agent is embedded in the devbox binary and pushed into every
// box, so a few hundred lines beats several megabytes of dependency, and the
// two things actually needed — a question name and a ClientHello SNI — are
// small, well-specified, and easy to test exhaustively.

// ErrMalformed is returned for input that is not a well-formed message.
var ErrMalformed = errors.New("decode: malformed message")

// DNSMessage is the part of a DNS message the observability plane cares about.
type DNSMessage struct {
	// ID is the transaction id, used to pair a response with its query.
	ID uint16
	// Response is true for an answer, false for a question.
	Response bool
	QName    string
	QType    string
	// Answers holds A/AAAA record data, in wire order.
	Answers []string
	// CNAMEs holds any CNAME targets seen, which is how a name maps onto the
	// address that eventually answers for it.
	CNAMEs []string
}

// qtypeName maps the DNS TYPE codes worth naming.
func qtypeName(t uint16) string {
	switch t {
	case 1:
		return "A"
	case 2:
		return "NS"
	case 5:
		return "CNAME"
	case 6:
		return "SOA"
	case 12:
		return "PTR"
	case 15:
		return "MX"
	case 16:
		return "TXT"
	case 28:
		return "AAAA"
	case 33:
		return "SRV"
	case 65:
		return "HTTPS"
	default:
		return fmt.Sprintf("TYPE%d", t)
	}
}

// maxDNSNameLabels bounds compression-pointer following.
//
// A DNS name is at most 255 bytes and 127 labels; a message can nevertheless
// contain a pointer loop, and a parser that follows pointers without a budget
// hangs. This is that budget.
const maxDNSNameLabels = 128

// parseName reads a (possibly compressed) DNS name starting at off.
// Returns the name and the offset just past the name *in the original message*.
func parseName(msg []byte, off int) (string, int, error) {
	var (
		labels   []string
		jumped   bool
		next     int
		budget   = maxDNSNameLabels
		position = off
	)

	for {
		if budget <= 0 {
			return "", 0, fmt.Errorf("%w: DNS name has a pointer loop", ErrMalformed)
		}
		budget--

		if position >= len(msg) {
			return "", 0, fmt.Errorf("%w: DNS name runs past the message", ErrMalformed)
		}

		length := int(msg[position])
		switch {
		case length == 0:
			position++
			if !jumped {
				next = position
			}
			return strings.Join(labels, "."), next, nil

		case length&0xC0 == 0xC0: // compression pointer
			if position+1 >= len(msg) {
				return "", 0, fmt.Errorf("%w: truncated DNS compression pointer", ErrMalformed)
			}
			target := int(binary.BigEndian.Uint16(msg[position:position+2]) & 0x3FFF)
			if !jumped {
				next = position + 2
				jumped = true
			}
			if target >= len(msg) {
				return "", 0, fmt.Errorf("%w: DNS pointer past the message", ErrMalformed)
			}
			position = target

		case length&0xC0 != 0:
			return "", 0, fmt.Errorf("%w: reserved DNS label type", ErrMalformed)

		default:
			start := position + 1
			end := start + length
			if end > len(msg) {
				return "", 0, fmt.Errorf("%w: DNS label runs past the message", ErrMalformed)
			}
			labels = append(labels, string(msg[start:end]))
			position = end
		}
	}
}

// DNS parses a DNS query or response from a UDP payload.
func DNS(msg []byte) (*DNSMessage, error) {
	const headerLen = 12
	if len(msg) < headerLen {
		return nil, fmt.Errorf("%w: DNS message shorter than its header", ErrMalformed)
	}

	out := &DNSMessage{
		ID:       binary.BigEndian.Uint16(msg[0:2]),
		Response: msg[2]&0x80 != 0,
	}
	qdCount := int(binary.BigEndian.Uint16(msg[4:6]))
	anCount := int(binary.BigEndian.Uint16(msg[6:8]))

	off := headerLen
	for i := 0; i < qdCount; i++ {
		name, next, err := parseName(msg, off)
		if err != nil {
			return nil, err
		}
		off = next
		if off+4 > len(msg) {
			return nil, fmt.Errorf("%w: truncated DNS question", ErrMalformed)
		}
		if i == 0 {
			out.QName = name
			out.QType = qtypeName(binary.BigEndian.Uint16(msg[off : off+2]))
		}
		off += 4 // QTYPE + QCLASS
	}

	for i := 0; i < anCount; i++ {
		_, next, err := parseName(msg, off)
		if err != nil {
			return nil, err
		}
		off = next
		if off+10 > len(msg) {
			return nil, fmt.Errorf("%w: truncated DNS answer", ErrMalformed)
		}
		rrType := binary.BigEndian.Uint16(msg[off : off+2])
		rdLength := int(binary.BigEndian.Uint16(msg[off+8 : off+10]))
		off += 10

		if off+rdLength > len(msg) {
			return nil, fmt.Errorf("%w: DNS rdata runs past the message", ErrMalformed)
		}
		rdata := msg[off : off+rdLength]

		switch rrType {
		case 1: // A
			if rdLength == 4 {
				out.Answers = append(out.Answers,
					netip.AddrFrom4([4]byte(rdata)).String())
			}
		case 28: // AAAA
			if rdLength == 16 {
				out.Answers = append(out.Answers,
					netip.AddrFrom16([16]byte(rdata)).String())
			}
		case 5: // CNAME
			if name, _, err := parseName(msg, off); err == nil {
				out.CNAMEs = append(out.CNAMEs, name)
			}
		}
		off += rdLength
	}

	return out, nil
}

// ClientHello is what a TLS handshake reveals without decrypting anything.
type ClientHello struct {
	SNI  string
	ALPN string
}

// SNI parses a TLS ClientHello and extracts the server name and first ALPN
// protocol.
//
// `record` is a complete TLS record: the 5-byte record header followed by the
// handshake message. No decryption is involved — the ClientHello is plaintext
// by design, which is exactly why it is the cheapest reliable way to learn who
// a box is talking to.
func SNI(record []byte) (*ClientHello, error) {
	// TLS record header: type(1) version(2) length(2)
	if len(record) < 5 {
		return nil, fmt.Errorf("%w: TLS record shorter than its header", ErrMalformed)
	}
	if record[0] != 0x16 {
		return nil, fmt.Errorf("%w: not a TLS handshake record", ErrMalformed)
	}
	recLen := int(binary.BigEndian.Uint16(record[3:5]))
	body := record[5:]
	if len(body) < recLen {
		return nil, fmt.Errorf("%w: TLS record is truncated", ErrMalformed)
	}
	body = body[:recLen]

	// Handshake header: type(1) length(3)
	if len(body) < 4 || body[0] != 0x01 {
		return nil, fmt.Errorf("%w: not a ClientHello", ErrMalformed)
	}
	hs := body[4:]

	// client_version(2) random(32)
	if len(hs) < 34 {
		return nil, fmt.Errorf("%w: ClientHello is truncated", ErrMalformed)
	}
	off := 34

	// session_id
	if off >= len(hs) {
		return nil, fmt.Errorf("%w: ClientHello has no session id", ErrMalformed)
	}
	off += 1 + int(hs[off])

	// cipher_suites
	if off+2 > len(hs) {
		return nil, fmt.Errorf("%w: ClientHello has no cipher suites", ErrMalformed)
	}
	off += 2 + int(binary.BigEndian.Uint16(hs[off:off+2]))

	// compression_methods
	if off >= len(hs) {
		return nil, fmt.Errorf("%w: ClientHello has no compression methods", ErrMalformed)
	}
	off += 1 + int(hs[off])

	// extensions
	if off+2 > len(hs) {
		// A ClientHello with no extensions is legal but tells us nothing.
		return &ClientHello{}, nil
	}
	extTotal := int(binary.BigEndian.Uint16(hs[off : off+2]))
	off += 2
	if off+extTotal > len(hs) {
		return nil, fmt.Errorf("%w: ClientHello extensions run past the record", ErrMalformed)
	}
	ext := hs[off : off+extTotal]

	out := &ClientHello{}
	for len(ext) >= 4 {
		extType := binary.BigEndian.Uint16(ext[0:2])
		extLen := int(binary.BigEndian.Uint16(ext[2:4]))
		if 4+extLen > len(ext) {
			return nil, fmt.Errorf("%w: TLS extension runs past the block", ErrMalformed)
		}
		data := ext[4 : 4+extLen]

		switch extType {
		case 0x0000: // server_name
			if name, ok := parseSNIExtension(data); ok {
				out.SNI = name
			}
		case 0x0010: // application_layer_protocol_negotiation
			if proto, ok := parseALPNExtension(data); ok {
				out.ALPN = proto
			}
		}
		ext = ext[4+extLen:]
	}
	return out, nil
}

// parseSNIExtension reads the first host_name entry from a server_name list.
func parseSNIExtension(data []byte) (string, bool) {
	if len(data) < 2 {
		return "", false
	}
	listLen := int(binary.BigEndian.Uint16(data[0:2]))
	if 2+listLen > len(data) {
		return "", false
	}
	list := data[2 : 2+listLen]

	for len(list) >= 3 {
		nameType := list[0]
		nameLen := int(binary.BigEndian.Uint16(list[1:3]))
		if 3+nameLen > len(list) {
			return "", false
		}
		if nameType == 0 { // host_name
			return string(list[3 : 3+nameLen]), true
		}
		list = list[3+nameLen:]
	}
	return "", false
}

// parseALPNExtension reads the first offered protocol.
func parseALPNExtension(data []byte) (string, bool) {
	if len(data) < 2 {
		return "", false
	}
	listLen := int(binary.BigEndian.Uint16(data[0:2]))
	if 2+listLen > len(data) || listLen < 1 {
		return "", false
	}
	list := data[2 : 2+listLen]

	protoLen := int(list[0])
	if 1+protoLen > len(list) {
		return "", false
	}
	return string(list[1 : 1+protoLen]), true
}
