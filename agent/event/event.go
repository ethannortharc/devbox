// Package event defines the canonical observability event — §11.1 of the v4
// design.
//
// This schema is a contract between three components: the Go agent produces
// it, the Rust collector stores and queries it, and the web console renders
// it. §11.1 is explicitly called out as stable, so field names and types here
// change only alongside `src/obs/event.rs` and a protocol version bump.
//
// The envelope is uniform — timestamps, process identity, box identity — and
// each event type populates exactly one sub-object. That is what makes storage
// uniform and correlation (§7.2) a join rather than a special case per type.
package event

import (
	"encoding/json"
	"fmt"
	"time"
)

// Type classifies an event and selects which sub-object is populated.
type Type string

// The event types from §11.1. Anything not in this list is rejected at the
// collector rather than stored as an unclassifiable row.
const (
	TypeExec    Type = "exec"
	TypeExit    Type = "exit"
	TypeConnect Type = "connect"
	TypeAccept  Type = "accept"
	TypeDNS     Type = "dns"
	TypeTLS     Type = "tls"
	TypeFile    Type = "file"
	TypeSyscall Type = "syscall"
	TypeAPI     Type = "api"
	TypePolicy  Type = "policy"
)

// AllTypes lists every valid event type, in schema order.
var AllTypes = []Type{
	TypeExec, TypeExit, TypeConnect, TypeAccept, TypeDNS,
	TypeTLS, TypeFile, TypeSyscall, TypeAPI, TypePolicy,
}

// Valid reports whether t is a known event type.
func (t Type) Valid() bool {
	for _, known := range AllTypes {
		if t == known {
			return true
		}
	}
	return false
}

// SubObject names the field a type populates, or "" for types that carry only
// the envelope.
func (t Type) SubObject() string {
	switch t {
	case TypeExec:
		return "exec"
	case TypeConnect, TypeAccept, TypeDNS, TypeTLS:
		return "net"
	case TypeFile:
		return "file"
	case TypeAPI:
		return "api"
	case TypePolicy:
		return "policy"
	default:
		return ""
	}
}

// Event is the canonical envelope. Every signal the agent captures becomes one
// of these, whatever subsystem produced it.
type Event struct {
	// TSWall is the wall-clock timestamp, RFC 3339 with millisecond
	// precision. Used for display and for joining against anything outside
	// the box.
	TSWall string `json:"ts_wall"`
	// TSMonoNS is the kernel's monotonic clock in nanoseconds. This is the
	// ordering key: wall clock can step backwards (NTP), monotonic cannot,
	// and correlation depends on a total order that survives a clock jump.
	TSMonoNS uint64 `json:"ts_mono_ns"`

	BoxID    string `json:"box_id"`
	CgroupID uint64 `json:"cgroup_id"`

	PID  uint32 `json:"pid"`
	TID  uint32 `json:"tid"`
	PPID uint32 `json:"ppid"`
	Comm string `json:"comm"`
	UID  uint32 `json:"uid"`

	Type Type `json:"type"`

	// Exactly one sub-object is populated, chosen by Type. Absent rather than
	// null on the wire (ADR-0015): at 10k events/s the five null fields cost
	// more than they explain.
	Net    *Net    `json:"net,omitempty"`
	Exec   *Exec   `json:"exec,omitempty"`
	File   *File   `json:"file,omitempty"`
	API    *API    `json:"api,omitempty"`
	Policy *Policy `json:"policy,omitempty"`
}

// Net carries connection, DNS, and TLS detail.
type Net struct {
	Proto string `json:"proto"`
	SAddr string `json:"saddr"`
	SPort uint16 `json:"sport"`
	DAddr string `json:"daddr"`
	DPort uint16 `json:"dport"`

	// Domain is the DNS name that resolved to DAddr, filled in by the
	// agent's reverse map (§7.3) so the console can show `pypi.org` rather
	// than an address nobody recognizes.
	Domain string `json:"domain,omitempty"`
	SNI    string `json:"sni,omitempty"`
	ALPN   string `json:"alpn,omitempty"`

	// QName/QType/Answers are populated for `dns` events.
	QName   string   `json:"qname,omitempty"`
	QType   string   `json:"qtype,omitempty"`
	Answers []string `json:"answers,omitempty"`

	BytesTX uint64 `json:"bytes_tx,omitempty"`
	BytesRX uint64 `json:"bytes_rx,omitempty"`
	DurMS   uint64 `json:"dur_ms,omitempty"`
}

// Exec carries process-execution detail.
type Exec struct {
	Path string   `json:"path"`
	Argv []string `json:"argv,omitempty"`
	CWD  string   `json:"cwd,omitempty"`
}

// File carries file-access detail.
type File struct {
	Path string `json:"path"`
	// Op is one of open, create, write, unlink, rename.
	Op    string `json:"op"`
	Flags uint32 `json:"flags,omitempty"`
}

// API carries application-level detail (opt-in, §7.1).
type API struct {
	Method   string `json:"method,omitempty"`
	Host     string `json:"host,omitempty"`
	Path     string `json:"path,omitempty"`
	Status   uint16 `json:"status,omitempty"`
	Tokens   uint64 `json:"tokens,omitempty"`
	Endpoint string `json:"endpoint,omitempty"`
}

// Policy carries an egress-policy decision (§8).
type Policy struct {
	// Verdict is allow, block, or flag.
	Verdict string `json:"verdict"`
	// Mode is the posture in force: open, allowlist, mirror-only, isolated.
	Mode   string `json:"mode"`
	Target string `json:"target,omitempty"`
	Reason string `json:"reason,omitempty"`
}

// Now formats a time the way TSWall expects.
//
// Millisecond precision, UTC, `Z` suffix — matching the example in §11.1 and,
// crucially, sorting lexicographically in the same order as chronologically,
// which makes a plain string column a usable index.
func Now(t time.Time) string {
	return t.UTC().Format("2006-01-02T15:04:05.000Z")
}

// Validate reports whether an event is well-formed enough to store.
//
// The collector rejects rather than stores malformed events: a row that cannot
// be interpreted is worse than a dropped-event counter, because it silently
// corrupts every later query.
func (e *Event) Validate() error {
	if !e.Type.Valid() {
		return fmt.Errorf("unknown event type %q", e.Type)
	}
	if e.TSWall == "" {
		return fmt.Errorf("ts_wall is required")
	}
	if e.BoxID == "" {
		return fmt.Errorf("box_id is required")
	}
	if e.PID == 0 {
		return fmt.Errorf("pid is required")
	}

	switch e.Type.SubObject() {
	case "net":
		if e.Net == nil {
			return fmt.Errorf("%s event requires a net sub-object", e.Type)
		}
	case "exec":
		if e.Exec == nil {
			return fmt.Errorf("exec event requires an exec sub-object")
		}
	case "file":
		if e.File == nil {
			return fmt.Errorf("file event requires a file sub-object")
		}
	case "api":
		if e.API == nil {
			return fmt.Errorf("api event requires an api sub-object")
		}
	case "policy":
		if e.Policy == nil {
			return fmt.Errorf("policy event requires a policy sub-object")
		}
	}
	return nil
}

// MarshalJSON is the wire encoding. Declared explicitly so the encoding is a
// named part of the contract rather than an implementation detail.
func (e *Event) Encode() ([]byte, error) {
	return json.Marshal(e)
}

// Decode parses one event from its wire encoding.
func Decode(data []byte) (*Event, error) {
	var e Event
	if err := json.Unmarshal(data, &e); err != nil {
		return nil, fmt.Errorf("decode event: %w", err)
	}
	return &e, nil
}
