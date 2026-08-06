// Package decode turns raw kernel ring-buffer records into canonical events.
//
// The eBPF programs in `agent/bpf` write fixed-layout C structs into a
// perf/ring buffer; this package is the other half of that contract. The
// layouts are declared here as Go structs with explicit sizes, and asserted in
// tests — a silent layout drift between the C and Go sides would not fail to
// compile, it would produce plausible-looking garbage.
//
// Nothing here needs a kernel, so it is fully unit-tested on any host
// (§17: "decoder unit tests use captured fixtures so most Go tests need no
// privileges").
package decode

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"net/netip"
	"strings"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

// Byte order of every record: the kernel writes native-endian, and every
// platform devbox supports for the guest (x86-64, arm64) is little-endian.
var order = binary.LittleEndian

// Fixed buffer sizes, matching the C structs.
const (
	commLen     = 16
	filenameLen = 256
	argvLen     = 512
	pathLen     = 256
)

// Wire sizes. Asserted in tests so a struct change cannot silently pass.
const (
	ExecRecordSize = 820
	NetRecordSize  = 112
	FileRecordSize = 312
)

// Direction of a network record.
const (
	DirConnect uint8 = 0
	DirAccept  uint8 = 1
)

// File operations, matching the C enum.
const (
	FileOpen   uint32 = 0
	FileCreate uint32 = 1
	FileWrite  uint32 = 2
	FileUnlink uint32 = 3
	FileRename uint32 = 4
)

// ExecRecord mirrors `struct exec_event` in the eBPF program.
type ExecRecord struct {
	TSMonoNS uint64
	CgroupID uint64
	PID      uint32
	TID      uint32
	PPID     uint32
	UID      uint32
	Comm     [commLen]byte
	Filename [filenameLen]byte
	ArgvLen  uint32
	Argv     [argvLen]byte // NUL-separated argument vector
}

// NetRecord mirrors `struct net_event`.
type NetRecord struct {
	TSMonoNS  uint64
	CgroupID  uint64
	PID       uint32
	TID       uint32
	PPID      uint32
	UID       uint32
	Comm      [commLen]byte
	Family    uint8 // 2 = AF_INET, 10 = AF_INET6
	Proto     uint8 // 6 = TCP, 17 = UDP
	Direction uint8
	_         uint8
	SPort     uint16
	DPort     uint16
	SAddr     [16]byte
	DAddr     [16]byte
	BytesTX   uint64
	BytesRX   uint64
	DurNS     uint64
}

// FileRecord mirrors `struct file_event`.
type FileRecord struct {
	TSMonoNS uint64
	CgroupID uint64
	PID      uint32
	TID      uint32
	PPID     uint32
	UID      uint32
	Comm     [commLen]byte
	Flags    uint32
	Op       uint32
	Path     [pathLen]byte
}

// cstr trims a fixed-width C buffer at its first NUL.
func cstr(b []byte) string {
	if i := bytes.IndexByte(b, 0); i >= 0 {
		return string(b[:i])
	}
	return string(b)
}

// splitArgv splits the NUL-separated argument buffer.
//
// The kernel truncates at the buffer size, so a trailing partial argument is
// possible; it is kept rather than dropped, because "…" in the console is more
// honest than a silently shorter command line.
func splitArgv(b []byte, n uint32) []string {
	if n > uint32(len(b)) {
		n = uint32(len(b))
	}
	raw := b[:n]
	parts := bytes.Split(raw, []byte{0})

	out := make([]string, 0, len(parts))
	for _, p := range parts {
		if len(p) == 0 {
			continue
		}
		out = append(out, string(p))
	}
	return out
}

// addr renders a raw address buffer per address family.
func addr(family uint8, raw [16]byte) string {
	switch family {
	case 2: // AF_INET
		return netip.AddrFrom4([4]byte(raw[:4])).String()
	case 10: // AF_INET6
		return netip.AddrFrom16(raw).Unmap().String()
	default:
		return ""
	}
}

func protoName(p uint8) string {
	switch p {
	case 6:
		return "tcp"
	case 17:
		return "udp"
	default:
		return fmt.Sprintf("proto-%d", p)
	}
}

func fileOpName(op uint32) string {
	switch op {
	case FileOpen:
		return "open"
	case FileCreate:
		return "create"
	case FileWrite:
		return "write"
	case FileUnlink:
		return "unlink"
	case FileRename:
		return "rename"
	default:
		return "unknown"
	}
}

// Clock converts a kernel monotonic timestamp into a wall-clock string.
//
// The kernel only has monotonic nanoseconds since boot; the agent captures the
// wall/monotonic offset once at startup and applies it here, which keeps a
// mid-run NTP step from making events appear to travel backwards.
type Clock struct {
	// BootWall is the wall-clock time corresponding to monotonic zero.
	BootWall time.Time
}

// Wall renders the wall-clock timestamp for a monotonic reading.
func (c Clock) Wall(monoNS uint64) string {
	return event.Now(c.BootWall.Add(time.Duration(monoNS)))
}

// DecodeExec parses an exec record and builds the canonical event.
func DecodeExec(raw []byte, boxID string, clock Clock) (*event.Event, error) {
	var r ExecRecord
	if err := binary.Read(bytes.NewReader(raw), order, &r); err != nil {
		return nil, fmt.Errorf("decode exec record (%d bytes): %w", len(raw), err)
	}

	return &event.Event{
		TSWall:   clock.Wall(r.TSMonoNS),
		TSMonoNS: r.TSMonoNS,
		BoxID:    boxID,
		CgroupID: r.CgroupID,
		PID:      r.PID, TID: r.TID, PPID: r.PPID, UID: r.UID,
		Comm: cstr(r.Comm[:]),
		Type: event.TypeExec,
		Exec: &event.Exec{
			Path: cstr(r.Filename[:]),
			Argv: splitArgv(r.Argv[:], r.ArgvLen),
		},
	}, nil
}

// DecodeNet parses a connect/accept record.
func DecodeNet(raw []byte, boxID string, clock Clock) (*event.Event, error) {
	var r NetRecord
	if err := binary.Read(bytes.NewReader(raw), order, &r); err != nil {
		return nil, fmt.Errorf("decode net record (%d bytes): %w", len(raw), err)
	}

	ty := event.TypeConnect
	if r.Direction == DirAccept {
		ty = event.TypeAccept
	}

	return &event.Event{
		TSWall:   clock.Wall(r.TSMonoNS),
		TSMonoNS: r.TSMonoNS,
		BoxID:    boxID,
		CgroupID: r.CgroupID,
		PID:      r.PID, TID: r.TID, PPID: r.PPID, UID: r.UID,
		Comm: cstr(r.Comm[:]),
		Type: ty,
		Net: &event.Net{
			Proto: protoName(r.Proto),
			SAddr: addr(r.Family, r.SAddr), SPort: r.SPort,
			DAddr: addr(r.Family, r.DAddr), DPort: r.DPort,
			BytesTX: r.BytesTX, BytesRX: r.BytesRX,
			DurMS: r.DurNS / 1_000_000,
		},
	}, nil
}

// DecodeFile parses a file-access record.
func DecodeFile(raw []byte, boxID string, clock Clock) (*event.Event, error) {
	var r FileRecord
	if err := binary.Read(bytes.NewReader(raw), order, &r); err != nil {
		return nil, fmt.Errorf("decode file record (%d bytes): %w", len(raw), err)
	}

	return &event.Event{
		TSWall:   clock.Wall(r.TSMonoNS),
		TSMonoNS: r.TSMonoNS,
		BoxID:    boxID,
		CgroupID: r.CgroupID,
		PID:      r.PID, TID: r.TID, PPID: r.PPID, UID: r.UID,
		Comm: cstr(r.Comm[:]),
		Type: event.TypeFile,
		File: &event.File{
			Path:  cstr(r.Path[:]),
			Op:    fileOpName(r.Op),
			Flags: r.Flags,
		},
	}, nil
}

// Encode is the inverse of DecodeExec, used to build fixtures.
func (r *ExecRecord) Encode() []byte {
	var buf bytes.Buffer
	_ = binary.Write(&buf, order, r)
	return buf.Bytes()
}

// Encode is the inverse of DecodeNet, used to build fixtures.
func (r *NetRecord) Encode() []byte {
	var buf bytes.Buffer
	_ = binary.Write(&buf, order, r)
	return buf.Bytes()
}

// Encode is the inverse of DecodeFile, used to build fixtures.
func (r *FileRecord) Encode() []byte {
	var buf bytes.Buffer
	_ = binary.Write(&buf, order, r)
	return buf.Bytes()
}

// FillString copies s into a fixed-width buffer, NUL-terminating it.
func FillString(dst []byte, s string) {
	n := copy(dst, s)
	if n < len(dst) {
		dst[n] = 0
	}
}

// PackArgv renders an argument vector into the NUL-separated buffer layout,
// returning the number of bytes used.
func PackArgv(dst []byte, argv []string) uint32 {
	joined := strings.Join(argv, "\x00")
	n := copy(dst, joined)
	return uint32(n)
}
