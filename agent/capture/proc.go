package capture

import (
	"bufio"
	"context"
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"net/netip"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

// Proc is the degraded capture path (§13): poll `/proc` instead of attaching
// eBPF programs.
//
// It exists for kernels without BTF, and it is honest about what it costs. A
// poll sees a process only if it is still alive when the poll runs, so a
// short-lived `ls` is invisible; eBPF sees every `execve` because the kernel
// tells it. The console marks which source is in play so nobody reads an empty
// timeline as "nothing happened".
type Proc struct {
	// Root is the procfs mount, overridable for tests.
	Root string
	// BoxID stamps every event.
	BoxID string
	// Interval between polls.
	Interval time.Duration
	// Boot is the wall-clock time of monotonic zero, for timestamping.
	Boot time.Time
}

// DefaultProcInterval balances missing short-lived processes against burning
// CPU walking /proc. 250ms catches anything human-scale.
const DefaultProcInterval = 250 * time.Millisecond

// Name implements Source.
func (p *Proc) Name() string { return "proc" }

// Domains implements Source.
//
// Deliberately short: polling can see processes and open sockets, and cannot
// see DNS, TLS, or file access without the kernel telling it.
func (p *Proc) Domains() []event.Type {
	return []event.Type{event.TypeExec, event.TypeExit, event.TypeConnect}
}

// Run implements Source.
func (p *Proc) Run(ctx context.Context, out chan<- *event.Event) error {
	if runtime.GOOS != "linux" {
		return &ErrUnsupported{
			Source: "proc",
			Reason: "procfs exists only on Linux; on macOS the agent runs inside the Lima guest",
		}
	}
	if p.Root == "" {
		p.Root = "/proc"
	}
	if p.Interval == 0 {
		p.Interval = DefaultProcInterval
	}

	known := map[int]ProcInfo{}
	ticker := time.NewTicker(p.Interval)
	defer ticker.Stop()

	for {
		current, err := ScanProcs(p.Root)
		if err != nil {
			return fmt.Errorf("scan %s: %w", p.Root, err)
		}

		for pid, info := range current {
			if _, seen := known[pid]; seen {
				continue
			}
			if err := Send(ctx, out, p.execEvent(info)); err != nil {
				return err
			}
		}
		for pid, info := range known {
			if _, alive := current[pid]; !alive {
				if err := Send(ctx, out, p.exitEvent(info)); err != nil {
					return err
				}
			}
		}
		known = current

		select {
		case <-ticker.C:
		case <-ctx.Done():
			return ctx.Err()
		}
	}
}

func (p *Proc) stamp(e *event.Event) *event.Event {
	now := time.Now()
	e.TSWall = event.Now(now)
	e.TSMonoNS = uint64(now.Sub(p.Boot).Nanoseconds())
	e.BoxID = p.BoxID
	return e
}

func (p *Proc) execEvent(info ProcInfo) *event.Event {
	return p.stamp(&event.Event{
		PID:  uint32(info.PID),
		TID:  uint32(info.PID),
		PPID: uint32(info.PPID),
		Comm: info.Comm,
		UID:  info.UID,
		Type: event.TypeExec,
		Exec: &event.Exec{
			Path: info.Exe,
			Argv: info.Argv,
			CWD:  info.CWD,
		},
	})
}

func (p *Proc) exitEvent(info ProcInfo) *event.Event {
	return p.stamp(&event.Event{
		PID:  uint32(info.PID),
		TID:  uint32(info.PID),
		PPID: uint32(info.PPID),
		Comm: info.Comm,
		UID:  info.UID,
		Type: event.TypeExit,
	})
}

// ProcInfo is what a single /proc/<pid> entry yields.
type ProcInfo struct {
	PID  int
	PPID int
	UID  uint32
	Comm string
	Exe  string
	CWD  string
	Argv []string
}

// ScanProcs reads every numeric entry under root.
//
// Processes that vanish mid-scan are skipped rather than erroring: that is the
// normal case, not a failure.
func ScanProcs(root string) (map[int]ProcInfo, error) {
	entries, err := os.ReadDir(root)
	if err != nil {
		return nil, err
	}

	out := make(map[int]ProcInfo, len(entries))
	for _, entry := range entries {
		pid, err := strconv.Atoi(entry.Name())
		if err != nil {
			continue // not a pid directory
		}
		info, err := ReadProc(root, pid)
		if err != nil {
			continue // exited between readdir and read
		}
		out[pid] = info
	}
	return out, nil
}

// ReadProc reads one process's details.
func ReadProc(root string, pid int) (ProcInfo, error) {
	dir := filepath.Join(root, strconv.Itoa(pid))

	statusRaw, err := os.ReadFile(filepath.Join(dir, "status"))
	if err != nil {
		return ProcInfo{}, err
	}
	info := ParseStatus(string(statusRaw))
	info.PID = pid

	if cmdline, err := os.ReadFile(filepath.Join(dir, "cmdline")); err == nil {
		info.Argv = ParseCmdline(cmdline)
	}
	if exe, err := os.Readlink(filepath.Join(dir, "exe")); err == nil {
		info.Exe = exe
	} else if len(info.Argv) > 0 {
		// A kernel thread has no exe link; the argv is the best name there is.
		info.Exe = info.Argv[0]
	}
	if cwd, err := os.Readlink(filepath.Join(dir, "cwd")); err == nil {
		info.CWD = cwd
	}
	return info, nil
}

// ParseStatus pulls Name, PPid, and the real Uid out of /proc/<pid>/status.
func ParseStatus(text string) ProcInfo {
	var info ProcInfo
	for _, line := range strings.Split(text, "\n") {
		name, value, ok := strings.Cut(line, ":")
		if !ok {
			continue
		}
		value = strings.TrimSpace(value)
		switch name {
		case "Name":
			info.Comm = value
		case "PPid":
			info.PPID, _ = strconv.Atoi(value)
		case "Uid":
			// "Uid:\treal\teffective\tsaved\tfs" — the real uid is who ran it.
			if fields := strings.Fields(value); len(fields) > 0 {
				if uid, err := strconv.ParseUint(fields[0], 10, 32); err == nil {
					info.UID = uint32(uid)
				}
			}
		}
	}
	return info
}

// ParseCmdline splits the NUL-separated /proc/<pid>/cmdline.
func ParseCmdline(raw []byte) []string {
	parts := strings.Split(string(raw), "\x00")
	out := make([]string, 0, len(parts))
	for _, p := range parts {
		if p != "" {
			out = append(out, p)
		}
	}
	return out
}

// ParseNetTCP parses /proc/net/tcp (or tcp6) into established connections.
//
// The hex address encoding is little-endian per 32-bit word, which is the
// detail every naive parser of this file gets wrong.
func ParseNetTCP(text string, v6 bool) ([]Conn, error) {
	var out []Conn
	scanner := bufio.NewScanner(strings.NewReader(text))
	first := true

	for scanner.Scan() {
		if first { // header
			first = false
			continue
		}
		fields := strings.Fields(scanner.Text())
		if len(fields) < 8 {
			continue
		}

		local, lport, err := parseHexAddr(fields[1], v6)
		if err != nil {
			continue
		}
		remote, rport, err := parseHexAddr(fields[2], v6)
		if err != nil {
			continue
		}
		state, err := strconv.ParseUint(fields[3], 16, 8)
		if err != nil {
			continue
		}
		inode, _ := strconv.ParseUint(fields[9], 10, 64)

		out = append(out, Conn{
			LocalAddr: local, LocalPort: lport,
			RemoteAddr: remote, RemotePort: rport,
			State: uint8(state), Inode: inode,
		})
	}
	return out, scanner.Err()
}

// Conn is one row of /proc/net/tcp.
type Conn struct {
	LocalAddr  string
	LocalPort  uint16
	RemoteAddr string
	RemotePort uint16
	// State is the TCP state; 1 is ESTABLISHED.
	State uint8
	Inode uint64
}

// TCPEstablished is the state value for an established connection.
const TCPEstablished uint8 = 1

func parseHexAddr(field string, v6 bool) (string, uint16, error) {
	addrHex, portHex, ok := strings.Cut(field, ":")
	if !ok {
		return "", 0, fmt.Errorf("malformed address %q", field)
	}
	port, err := strconv.ParseUint(portHex, 16, 16)
	if err != nil {
		return "", 0, fmt.Errorf("malformed port in %q: %w", field, err)
	}

	raw, err := hex.DecodeString(addrHex)
	if err != nil {
		return "", 0, fmt.Errorf("malformed address in %q: %w", field, err)
	}

	// Each 4-byte group is a little-endian u32.
	for i := 0; i+4 <= len(raw); i += 4 {
		word := binary.LittleEndian.Uint32(raw[i : i+4])
		binary.BigEndian.PutUint32(raw[i:i+4], word)
	}

	switch {
	case v6 && len(raw) == 16:
		return netip.AddrFrom16([16]byte(raw)).Unmap().String(), uint16(port), nil
	case !v6 && len(raw) == 4:
		return netip.AddrFrom4([4]byte(raw)).String(), uint16(port), nil
	default:
		return "", 0, fmt.Errorf("address %q has the wrong width for the family", field)
	}
}
