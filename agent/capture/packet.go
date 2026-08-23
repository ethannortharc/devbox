package capture

import (
	"encoding/binary"
	"net/netip"
	"strings"
	"time"

	"github.com/ethannortharc/devbox/agent/decode"
	"github.com/ethannortharc/devbox/agent/event"
)

// Packet captures the payload-bearing network events eBPF syscall probes do
// not have: DNS messages and plaintext TLS ClientHello metadata.
//
// It is deliberately a separate source. Exec/file correlation needs kernel
// hooks, while DNS and SNI need packet bytes; pretending either mechanism can
// supply both is how the handshake previously advertised feeds that had no
// producer.
type Packet struct {
	BoxID     string
	Boot      time.Time
	Resolvers map[netip.Addr]struct{}
	pending   map[dnsQueryKey]time.Time
}

const (
	maxPendingDNS = 4096
	dnsQueryTTL   = 30 * time.Second
)

type dnsQueryKey struct {
	resolver   netip.Addr
	clientPort uint16
	id         uint16
	name       string
	qtype      string
}

// Name identifies the payload capture source in the agent handshake and logs.
func (p *Packet) Name() string { return "packet" }

// Domains reports the event types this packet source can actually emit.
func (p *Packet) Domains() []event.Type {
	return []event.Type{event.TypeDNS, event.TypeTLS}
}

type packetView struct {
	proto        string
	saddr, daddr string
	sport, dport uint16
	payload      []byte
}

// decodePacket turns one Ethernet frame into zero or one schema events.
// Irrelevant and malformed traffic is ignored: a hostile packet must not stop
// capture for the entire box.
func (p *Packet) decodePacket(frame []byte, now time.Time) *event.Event {
	view, ok := networkPayload(frame)
	if !ok || len(view.payload) == 0 {
		return nil
	}

	netEvent := &event.Net{
		Proto: view.proto,
		SAddr: view.saddr,
		SPort: view.sport,
		DAddr: view.daddr,
		DPort: view.dport,
	}
	kind := event.Type("")

	if view.sport == 53 || view.dport == 53 {
		payload := view.payload
		if view.proto == "tcp" {
			if len(payload) < 2 {
				return nil
			}
			size := int(binary.BigEndian.Uint16(payload[:2]))
			if size == 0 || size > len(payload)-2 {
				return nil
			}
			payload = payload[2 : 2+size]
		}
		message, err := decode.DNS(payload)
		if err != nil || message.QName == "" {
			return nil
		}
		if !p.trustDNS(view, message, now) {
			return nil
		}
		kind = event.TypeDNS
		netEvent.QName = message.QName
		netEvent.QType = message.QType
		netEvent.Answers = message.Answers
		netEvent.Response = message.Response
	} else if view.proto == "tcp" {
		hello, err := decode.SNI(view.payload)
		if err != nil || hello.SNI == "" {
			return nil
		}
		kind = event.TypeTLS
		netEvent.SNI = hello.SNI
		netEvent.ALPN = hello.ALPN
	} else {
		return nil
	}

	mono := now.Sub(p.Boot)
	if mono < 0 {
		mono = 0
	}
	return &event.Event{
		TSWall:   event.Now(now),
		TSMonoNS: uint64(mono.Nanoseconds()),
		BoxID:    p.BoxID,
		PID:      UnattributedPID,
		TID:      UnattributedPID,
		Comm:     "packet-tap",
		Type:     kind,
		Net:      netEvent,
	}
}

// trustDNS accepts queries sent by the box to its configured resolvers and
// only accepts an answer when it matches one of those observed queries.
//
// AF_PACKET sees frames before the input firewall. Port 53 alone is therefore
// not an authority signal: without this correlation, a dropped remote packet
// or an unprivileged local sendto could populate the nftables allow set.
func (p *Packet) trustDNS(view packetView, message *decode.DNSMessage, now time.Time) bool {
	resolverText := view.daddr
	clientPort := view.sport
	if message.Response {
		resolverText = view.saddr
		clientPort = view.dport
	}
	resolver, err := netip.ParseAddr(resolverText)
	if err != nil {
		return false
	}
	if _, trusted := p.Resolvers[resolver.Unmap()]; !trusted {
		return false
	}

	if p.pending == nil {
		p.pending = make(map[dnsQueryKey]time.Time)
	}
	key := dnsQueryKey{
		resolver:   resolver.Unmap(),
		clientPort: clientPort,
		id:         message.ID,
		name:       strings.ToLower(strings.TrimSuffix(message.QName, ".")),
		qtype:      message.QType,
	}

	for candidate, expires := range p.pending {
		if !expires.After(now) {
			delete(p.pending, candidate)
		}
	}

	if !message.Response {
		if view.dport != 53 || view.sport == 53 {
			return false
		}
		if len(p.pending) >= maxPendingDNS {
			// Bounded under a query flood. Which oldest entry is discarded is not
			// security-sensitive; an unmatched answer is denied.
			for candidate := range p.pending {
				delete(p.pending, candidate)
				break
			}
		}
		p.pending[key] = now.Add(dnsQueryTTL)
		return true
	}

	if view.sport != 53 || view.dport == 53 {
		return false
	}
	expires, matched := p.pending[key]
	delete(p.pending, key) // a transaction answer is accepted at most once
	return matched && expires.After(now)
}

func networkPayload(frame []byte) (packetView, bool) {
	if len(frame) < 14 {
		return packetView{}, false
	}
	offset := 14
	etherType := binary.BigEndian.Uint16(frame[12:14])
	// VLAN and provider bridging headers may be stacked.
	for etherType == 0x8100 || etherType == 0x88a8 {
		if len(frame) < offset+4 {
			return packetView{}, false
		}
		etherType = binary.BigEndian.Uint16(frame[offset+2 : offset+4])
		offset += 4
	}

	var view packetView
	var transport uint8
	switch etherType {
	case 0x0800:
		if len(frame) < offset+20 || frame[offset]>>4 != 4 {
			return packetView{}, false
		}
		ihl := int(frame[offset]&0x0f) * 4
		if ihl < 20 || len(frame) < offset+ihl {
			return packetView{}, false
		}
		// Only the first fragment carries a transport header.
		if binary.BigEndian.Uint16(frame[offset+6:offset+8])&0x1fff != 0 {
			return packetView{}, false
		}
		transport = frame[offset+9]
		view.saddr = netip.AddrFrom4([4]byte(frame[offset+12 : offset+16])).String()
		view.daddr = netip.AddrFrom4([4]byte(frame[offset+16 : offset+20])).String()
		offset += ihl

	case 0x86dd:
		if len(frame) < offset+40 || frame[offset]>>4 != 6 {
			return packetView{}, false
		}
		transport = frame[offset+6]
		view.saddr = netip.AddrFrom16([16]byte(frame[offset+8 : offset+24])).String()
		view.daddr = netip.AddrFrom16([16]byte(frame[offset+24 : offset+40])).String()
		offset += 40
		// Walk the fixed-shape extension headers which commonly precede TCP or
		// UDP. ESP is encrypted and AH has a different shape, so neither can
		// reveal DNS/TLS payloads here.
		for transport == 0 || transport == 43 || transport == 60 || transport == 44 {
			if len(frame) < offset+8 {
				return packetView{}, false
			}
			next := frame[offset]
			if transport == 44 {
				if binary.BigEndian.Uint16(frame[offset+2:offset+4])&0xfff8 != 0 {
					return packetView{}, false
				}
				offset += 8
			} else {
				size := (int(frame[offset+1]) + 1) * 8
				if len(frame) < offset+size {
					return packetView{}, false
				}
				offset += size
			}
			transport = next
		}

	default:
		return packetView{}, false
	}

	switch transport {
	case 17: // UDP
		if len(frame) < offset+8 {
			return packetView{}, false
		}
		view.proto = "udp"
		view.sport = binary.BigEndian.Uint16(frame[offset : offset+2])
		view.dport = binary.BigEndian.Uint16(frame[offset+2 : offset+4])
		udpLen := int(binary.BigEndian.Uint16(frame[offset+4 : offset+6]))
		if udpLen < 8 || len(frame) < offset+udpLen {
			return packetView{}, false
		}
		view.payload = frame[offset+8 : offset+udpLen]
	case 6: // TCP
		if len(frame) < offset+20 {
			return packetView{}, false
		}
		view.proto = "tcp"
		view.sport = binary.BigEndian.Uint16(frame[offset : offset+2])
		view.dport = binary.BigEndian.Uint16(frame[offset+2 : offset+4])
		headerLen := int(frame[offset+12]>>4) * 4
		if headerLen < 20 || len(frame) < offset+headerLen {
			return packetView{}, false
		}
		view.payload = frame[offset+headerLen:]
	default:
		return packetView{}, false
	}
	return view, true
}
