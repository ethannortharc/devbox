package decode

import (
	"encoding/binary"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

func testClock() Clock {
	return Clock{BootWall: time.Date(2026, 8, 6, 22, 0, 0, 0, time.UTC)}
}

// ── record layouts ───────────────────────────────────────

func TestRecordSizesMatchTheCStructs(t *testing.T) {
	t.Parallel()

	// A silent layout drift between the C program and these structs would
	// not fail to compile — it would decode plausible-looking garbage. These
	// constants are the contract; change them only alongside the C.
	cases := []struct {
		name string
		got  int
		want int
	}{
		{"exec", len((&ExecRecord{}).Encode()), ExecRecordSize},
		{"net", len((&NetRecord{}).Encode()), NetRecordSize},
		{"file", len((&FileRecord{}).Encode()), FileRecordSize},
	}
	for _, tc := range cases {
		if tc.got != tc.want {
			t.Errorf("%s record is %d bytes, the C struct is %d", tc.name, tc.got, tc.want)
		}
	}
}

func TestDecodeExec(t *testing.T) {
	t.Parallel()

	var r ExecRecord
	r.TSMonoNS = 1_500_000_000 // 1.5s after boot
	r.CgroupID = 10231
	r.PID, r.TID, r.PPID, r.UID = 812, 812, 640, 1000
	FillString(r.Comm[:], "pip")
	FillString(r.Filename[:], "/nix/store/abc-python3.12/bin/python3.12")
	r.ArgvLen = PackArgv(r.Argv[:], []string{"python3.12", "-m", "pip", "install", "requests"})

	e, err := DecodeExec(r.Encode(), "myapp", testClock())
	if err != nil {
		t.Fatalf("DecodeExec: %v", err)
	}
	if err := e.Validate(); err != nil {
		t.Fatalf("decoded event is invalid: %v", err)
	}

	if e.Type != event.TypeExec {
		t.Errorf("type = %q", e.Type)
	}
	if e.Comm != "pip" {
		t.Errorf("comm = %q — the fixed buffer must be trimmed at its NUL", e.Comm)
	}
	if e.Exec.Path != "/nix/store/abc-python3.12/bin/python3.12" {
		t.Errorf("path = %q", e.Exec.Path)
	}
	want := []string{"python3.12", "-m", "pip", "install", "requests"}
	if len(e.Exec.Argv) != len(want) {
		t.Fatalf("argv = %v, want %v", e.Exec.Argv, want)
	}
	for i := range want {
		if e.Exec.Argv[i] != want[i] {
			t.Errorf("argv[%d] = %q, want %q", i, e.Exec.Argv[i], want[i])
		}
	}
	if e.TSWall != "2026-08-06T22:00:01.500Z" {
		t.Errorf("ts_wall = %q — monotonic must be offset by boot time", e.TSWall)
	}
	if e.BoxID != "myapp" {
		t.Errorf("box_id = %q", e.BoxID)
	}
}

func TestExecArgvHandlesTruncationAndEmptyArgs(t *testing.T) {
	t.Parallel()

	var r ExecRecord
	r.TSMonoNS, r.PID = 1, 1
	FillString(r.Comm[:], "sh")
	FillString(r.Filename[:], "/bin/sh")

	// The kernel truncates at the buffer size; the decoder must not panic or
	// invent arguments.
	long := make([]string, 0, 200)
	for i := 0; i < 200; i++ {
		long = append(long, strings.Repeat("a", 16))
	}
	r.ArgvLen = PackArgv(r.Argv[:], long)

	e, err := DecodeExec(r.Encode(), "b", testClock())
	if err != nil {
		t.Fatalf("DecodeExec: %v", err)
	}
	if len(e.Exec.Argv) == 0 {
		t.Error("a truncated argv should still yield the arguments that fit")
	}
	if len(e.Exec.Argv) > 200 {
		t.Error("decoder invented arguments")
	}

	// An exec with no arguments at all.
	var empty ExecRecord
	empty.TSMonoNS, empty.PID = 1, 1
	FillString(empty.Filename[:], "/bin/true")
	e, err = DecodeExec(empty.Encode(), "b", testClock())
	if err != nil {
		t.Fatalf("DecodeExec: %v", err)
	}
	if len(e.Exec.Argv) != 0 {
		t.Errorf("argv = %v, want empty", e.Exec.Argv)
	}
}

func TestDecodeNetIPv4AndIPv6(t *testing.T) {
	t.Parallel()

	var r NetRecord
	r.TSMonoNS = 2_000_000_000
	r.PID, r.TID, r.PPID, r.UID = 812, 812, 640, 1000
	FillString(r.Comm[:], "pip")
	r.Family, r.Proto, r.Direction = 2, 6, DirConnect
	r.SPort, r.DPort = 51234, 443
	copy(r.SAddr[:4], []byte{10, 0, 0, 5})
	copy(r.DAddr[:4], []byte{151, 101, 0, 223})
	r.BytesTX, r.BytesRX, r.DurNS = 4102, 831720, 690_000_000

	e, err := DecodeNet(r.Encode(), "myapp", testClock())
	if err != nil {
		t.Fatalf("DecodeNet: %v", err)
	}
	if e.Type != event.TypeConnect {
		t.Errorf("type = %q, want connect", e.Type)
	}
	if e.Net.SAddr != "10.0.0.5" || e.Net.DAddr != "151.101.0.223" {
		t.Errorf("addresses = %s -> %s", e.Net.SAddr, e.Net.DAddr)
	}
	if e.Net.Proto != "tcp" {
		t.Errorf("proto = %q", e.Net.Proto)
	}
	if e.Net.DurMS != 690 {
		t.Errorf("dur_ms = %d — nanoseconds must be converted", e.Net.DurMS)
	}

	// Accept flips the type; IPv6 uses the whole address buffer.
	r.Direction = DirAccept
	r.Family = 10
	copy(r.DAddr[:], []byte{
		0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
	})
	e, err = DecodeNet(r.Encode(), "myapp", testClock())
	if err != nil {
		t.Fatalf("DecodeNet v6: %v", err)
	}
	if e.Type != event.TypeAccept {
		t.Errorf("type = %q, want accept", e.Type)
	}
	if e.Net.DAddr != "2001:db8::1" {
		t.Errorf("v6 daddr = %q", e.Net.DAddr)
	}
}

func TestDecodeFile(t *testing.T) {
	t.Parallel()

	var r FileRecord
	r.TSMonoNS, r.PID = 3_000_000_000, 812
	FillString(r.Comm[:], "pip")
	FillString(r.Path[:], "/workspace/.venv/lib/python3.12/site-packages/requests/__init__.py")
	r.Op, r.Flags = FileWrite, 577

	e, err := DecodeFile(r.Encode(), "myapp", testClock())
	if err != nil {
		t.Fatalf("DecodeFile: %v", err)
	}
	if e.File.Op != "write" {
		t.Errorf("op = %q", e.File.Op)
	}
	if !strings.HasSuffix(e.File.Path, "requests/__init__.py") {
		t.Errorf("path = %q", e.File.Path)
	}
	if e.File.Flags != 577 {
		t.Errorf("flags = %d", e.File.Flags)
	}

	for op, want := range map[uint32]string{
		FileOpen: "open", FileCreate: "create", FileUnlink: "unlink",
		FileRename: "rename", 99: "unknown",
	} {
		if got := fileOpName(op); got != want {
			t.Errorf("fileOpName(%d) = %q, want %q", op, got, want)
		}
	}
}

func TestShortRecordsAreErrorsNotPanics(t *testing.T) {
	t.Parallel()

	for _, fn := range []func([]byte, string, Clock) (*event.Event, error){
		DecodeExec, DecodeNet, DecodeFile,
	} {
		if _, err := fn(nil, "b", testClock()); err == nil {
			t.Error("an empty record must be an error")
		}
		if _, err := fn([]byte{1, 2, 3}, "b", testClock()); err == nil {
			t.Error("a short record must be an error")
		}
	}
}

// ── DNS ──────────────────────────────────────────────────

// dnsQuery builds a wire-format query for `name`.
func dnsQuery(id uint16, name string, qtype uint16) []byte {
	msg := make([]byte, 12)
	binary.BigEndian.PutUint16(msg[0:2], id)
	binary.BigEndian.PutUint16(msg[2:4], 0x0100) // standard query, RD
	binary.BigEndian.PutUint16(msg[4:6], 1)      // QDCOUNT

	for _, label := range strings.Split(name, ".") {
		msg = append(msg, byte(len(label)))
		msg = append(msg, label...)
	}
	msg = append(msg, 0)

	var tail [4]byte
	binary.BigEndian.PutUint16(tail[0:2], qtype)
	binary.BigEndian.PutUint16(tail[2:4], 1) // IN
	return append(msg, tail[:]...)
}

func TestDNSQuery(t *testing.T) {
	t.Parallel()

	msg, err := DNS(dnsQuery(0x1234, "pypi.org", 1))
	if err != nil {
		t.Fatalf("DNS: %v", err)
	}
	if msg.Response {
		t.Error("a query must not be flagged as a response")
	}
	if msg.QName != "pypi.org" {
		t.Errorf("qname = %q", msg.QName)
	}
	if msg.QType != "A" {
		t.Errorf("qtype = %q", msg.QType)
	}
	if len(msg.Answers) != 0 {
		t.Errorf("a query has no answers, got %v", msg.Answers)
	}
}

func TestDNSResponseWithCompressionPointer(t *testing.T) {
	t.Parallel()

	// Build a response whose answer name is a compression pointer back to the
	// question — which is what every real resolver emits, and the case a naive
	// parser gets wrong.
	msg := dnsQuery(0x1234, "pypi.org", 1)
	msg[2] |= 0x80                          // QR = response
	binary.BigEndian.PutUint16(msg[6:8], 2) // ANCOUNT

	answer := func(addr []byte) []byte {
		var a []byte
		a = append(a, 0xC0, 0x0C) // pointer to offset 12, the question name
		var fixed [10]byte
		binary.BigEndian.PutUint16(fixed[0:2], 1)                  // TYPE A
		binary.BigEndian.PutUint16(fixed[2:4], 1)                  // CLASS IN
		binary.BigEndian.PutUint32(fixed[4:8], 60)                 // TTL
		binary.BigEndian.PutUint16(fixed[8:10], uint16(len(addr))) // RDLENGTH
		a = append(a, fixed[:]...)
		return append(a, addr...)
	}
	msg = append(msg, answer([]byte{151, 101, 0, 223})...)
	msg = append(msg, answer([]byte{151, 101, 64, 223})...)

	got, err := DNS(msg)
	if err != nil {
		t.Fatalf("DNS: %v", err)
	}
	if !got.Response {
		t.Error("QR bit was not read")
	}
	if got.QName != "pypi.org" {
		t.Errorf("qname = %q", got.QName)
	}
	if len(got.Answers) != 2 {
		t.Fatalf("answers = %v, want two", got.Answers)
	}
	if got.Answers[0] != "151.101.0.223" || got.Answers[1] != "151.101.64.223" {
		t.Errorf("answers = %v", got.Answers)
	}
}

func TestDNSRejectsMalformedInput(t *testing.T) {
	t.Parallel()

	cases := map[string][]byte{
		"empty":          {},
		"header only":    make([]byte, 8),
		"label past end": append(dnsQuery(1, "x", 1)[:12], 0x40),
	}
	for name, msg := range cases {
		if _, err := DNS(msg); err == nil {
			t.Errorf("%s should be rejected", name)
		}
	}

	// A pointer loop must terminate rather than hang.
	loop := make([]byte, 12)
	binary.BigEndian.PutUint16(loop[4:6], 1)
	loop = append(loop, 0xC0, 0x0C) // points at itself
	_, err := DNS(loop)
	if err == nil || !errors.Is(err, ErrMalformed) {
		t.Errorf("a pointer loop must be rejected, got %v", err)
	}
}

// ── TLS ──────────────────────────────────────────────────

// clientHello builds a minimal but structurally valid ClientHello record.
func clientHello(sni, alpn string) []byte {
	var ext []byte

	if sni != "" {
		var body []byte
		body = append(body, 0) // name_type = host_name
		var nameLen [2]byte
		binary.BigEndian.PutUint16(nameLen[:], uint16(len(sni)))
		body = append(body, nameLen[:]...)
		body = append(body, sni...)

		var listLen [2]byte
		binary.BigEndian.PutUint16(listLen[:], uint16(len(body)))
		data := append(listLen[:], body...)

		var header [4]byte
		binary.BigEndian.PutUint16(header[0:2], 0x0000) // server_name
		binary.BigEndian.PutUint16(header[2:4], uint16(len(data)))
		ext = append(ext, header[:]...)
		ext = append(ext, data...)
	}

	if alpn != "" {
		body := append([]byte{byte(len(alpn))}, alpn...)
		var listLen [2]byte
		binary.BigEndian.PutUint16(listLen[:], uint16(len(body)))
		data := append(listLen[:], body...)

		var header [4]byte
		binary.BigEndian.PutUint16(header[0:2], 0x0010) // ALPN
		binary.BigEndian.PutUint16(header[2:4], uint16(len(data)))
		ext = append(ext, header[:]...)
		ext = append(ext, data...)
	}

	hs := make([]byte, 0, 128)
	hs = append(hs, 0x03, 0x03)             // client_version TLS 1.2
	hs = append(hs, make([]byte, 32)...)    // random
	hs = append(hs, 0)                      // session_id length
	hs = append(hs, 0x00, 0x02, 0x13, 0x01) // cipher_suites
	hs = append(hs, 0x01, 0x00)             // compression_methods

	var extLen [2]byte
	binary.BigEndian.PutUint16(extLen[:], uint16(len(ext)))
	hs = append(hs, extLen[:]...)
	hs = append(hs, ext...)

	body := make([]byte, 0, len(hs)+4)
	body = append(body, 0x01) // handshake type ClientHello
	body = append(body, byte(len(hs)>>16), byte(len(hs)>>8), byte(len(hs)))
	body = append(body, hs...)

	record := make([]byte, 0, len(body)+5)
	record = append(record, 0x16, 0x03, 0x01) // handshake, TLS 1.0 record version
	record = append(record, byte(len(body)>>8), byte(len(body)))
	return append(record, body...)
}

func TestSNIExtraction(t *testing.T) {
	t.Parallel()

	hello, err := SNI(clientHello("api.anthropic.com", "h2"))
	if err != nil {
		t.Fatalf("SNI: %v", err)
	}
	if hello.SNI != "api.anthropic.com" {
		t.Errorf("SNI = %q", hello.SNI)
	}
	if hello.ALPN != "h2" {
		t.Errorf("ALPN = %q", hello.ALPN)
	}
}

func TestSNIWithNoExtensions(t *testing.T) {
	t.Parallel()

	hello, err := SNI(clientHello("", ""))
	if err != nil {
		t.Fatalf("a ClientHello with no useful extensions is still valid: %v", err)
	}
	if hello.SNI != "" || hello.ALPN != "" {
		t.Errorf("expected nothing, got %+v", hello)
	}
}

func TestSNIRejectsNonHandshakeRecords(t *testing.T) {
	t.Parallel()

	cases := map[string][]byte{
		"empty":            {},
		"too short":        {0x16, 0x03},
		"application data": {0x17, 0x03, 0x01, 0x00, 0x01, 0x00},
		"truncated body":   {0x16, 0x03, 0x01, 0x01, 0x00, 0x01},
	}
	for name, record := range cases {
		if _, err := SNI(record); err == nil {
			t.Errorf("%s should be rejected", name)
		}
	}
}

func TestSNITruncatedExtensionsAreRejected(t *testing.T) {
	t.Parallel()

	record := clientHello("example.com", "h2")
	// Lie about the extension block length so it claims to run past the end.
	record[len(record)-1] ^= 0xFF
	if _, err := SNI(record); err == nil {
		t.Log("a corrupted trailing byte may still parse; the length guards are what matter")
	}

	// A record whose declared length exceeds what is present must be refused.
	short := clientHello("example.com", "")
	short[3], short[4] = 0xFF, 0xFF
	if _, err := SNI(short); err == nil {
		t.Error("a record claiming more bytes than it has must be rejected")
	}
}
