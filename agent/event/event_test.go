package event

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestEveryTypeIsValidAndUnique(t *testing.T) {
	t.Parallel()

	seen := map[Type]bool{}
	for _, ty := range AllTypes {
		if !ty.Valid() {
			t.Errorf("%q is in AllTypes but not Valid()", ty)
		}
		if seen[ty] {
			t.Errorf("%q appears twice in AllTypes", ty)
		}
		seen[ty] = true
	}
	if Type("nonsense").Valid() {
		t.Error("an unknown type must not validate")
	}
}

func TestSubObjectMapping(t *testing.T) {
	t.Parallel()

	cases := map[Type]string{
		TypeExec:    "exec",
		TypeConnect: "net",
		TypeAccept:  "net",
		TypeClose:   "net",
		TypeDNS:     "net",
		TypeTLS:     "net",
		TypeFile:    "file",
		TypeAPI:     "api",
		TypePolicy:  "policy",
		// exit and syscall carry only the envelope.
		TypeExit:    "",
		TypeSyscall: "",
	}
	for ty, want := range cases {
		if got := ty.SubObject(); got != want {
			t.Errorf("%s.SubObject() = %q, want %q", ty, got, want)
		}
	}
}

func TestNowIsMillisecondUTC(t *testing.T) {
	t.Parallel()

	ts := Now(time.Date(2026, 8, 6, 22, 14, 7, 412_000_000, time.UTC))
	if ts != "2026-08-06T22:14:07.412Z" {
		t.Errorf("Now() = %q, want the §11.1 format", ts)
	}

	// A non-UTC input must still render as UTC.
	loc := time.FixedZone("test", 5*3600)
	ts = Now(time.Date(2026, 8, 6, 22, 0, 0, 0, loc))
	if ts != "2026-08-06T17:00:00.000Z" {
		t.Errorf("Now() did not normalize to UTC: %q", ts)
	}
}

func TestTimestampsSortLexicographically(t *testing.T) {
	t.Parallel()

	base := time.Date(2026, 8, 6, 22, 14, 7, 0, time.UTC)
	earlier := Now(base)
	later := Now(base.Add(time.Millisecond))
	if earlier >= later {
		t.Errorf("timestamps must sort as strings: %q !< %q", earlier, later)
	}
	// Across a second boundary too.
	if Now(base.Add(999*time.Millisecond)) >= Now(base.Add(time.Second)) {
		t.Error("timestamps must sort across a second boundary")
	}
}

func connectEvent() *Event {
	return &Event{
		TSWall:   "2026-08-06T22:14:07.412Z",
		TSMonoNS: 84021399123,
		BoxID:    "myapp",
		CgroupID: 10231,
		PID:      812, TID: 812, PPID: 640, Comm: "pip", UID: 1000,
		Type: TypeConnect,
		Net: &Net{
			Proto: "tcp",
			SAddr: "10.0.0.5", SPort: 51234,
			DAddr: "151.101.0.223", DPort: 443,
			Domain: "pypi.org", SNI: "pypi.org", ALPN: "h2",
			BytesTX: 4102, BytesRX: 831720, DurMS: 690,
		},
	}
}

func TestRoundTrip(t *testing.T) {
	t.Parallel()

	original := connectEvent()
	data, err := original.Encode()
	if err != nil {
		t.Fatalf("Encode: %v", err)
	}

	back, err := Decode(data)
	if err != nil {
		t.Fatalf("Decode: %v", err)
	}
	if back.Net == nil {
		t.Fatal("net sub-object was lost")
	}
	if back.Net.Domain != "pypi.org" || back.Net.DPort != 443 {
		t.Errorf("net detail changed: %+v", back.Net)
	}
	if back.TSMonoNS != original.TSMonoNS || back.PID != original.PID {
		t.Errorf("envelope changed: %+v", back)
	}
}

func TestWireFieldNamesMatchTheDesign(t *testing.T) {
	t.Parallel()

	data, err := connectEvent().Encode()
	if err != nil {
		t.Fatalf("Encode: %v", err)
	}

	var raw map[string]json.RawMessage
	if err := json.Unmarshal(data, &raw); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	// §11.1 names these exactly. Renaming any of them breaks the Rust
	// collector, the SQLite schema, and the console at once.
	for _, field := range []string{
		"ts_wall", "ts_mono_ns", "box_id", "cgroup_id",
		"pid", "tid", "ppid", "comm", "uid", "type", "net",
	} {
		if _, ok := raw[field]; !ok {
			t.Errorf("wire format is missing %q", field)
		}
	}

	// Sub-objects the event does not use are absent, not null (ADR-0015).
	for _, field := range []string{"exec", "file", "api", "policy"} {
		if _, ok := raw[field]; ok {
			t.Errorf("%q should be omitted on a connect event", field)
		}
	}
}

func TestValidateRequiresTheEnvelope(t *testing.T) {
	t.Parallel()

	cases := []struct {
		name  string
		mutta func(*Event)
		want  string
	}{
		{"unknown type", func(e *Event) { e.Type = "nope" }, "unknown event type"},
		{"no timestamp", func(e *Event) { e.TSWall = "" }, "ts_wall"},
		{"no box", func(e *Event) { e.BoxID = "" }, "box_id"},
		{"no pid", func(e *Event) { e.PID = 0 }, "pid"},
		{"missing sub-object", func(e *Event) { e.Net = nil }, "net sub-object"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			e := connectEvent()
			tc.mutta(e)
			err := e.Validate()
			if err == nil {
				t.Fatalf("expected an error mentioning %q", tc.want)
			}
			if !contains(err.Error(), tc.want) {
				t.Errorf("error %q should mention %q", err, tc.want)
			}
		})
	}

	if err := connectEvent().Validate(); err != nil {
		t.Errorf("a well-formed event must validate: %v", err)
	}
}

func TestValidateChecksEachSubObject(t *testing.T) {
	t.Parallel()

	base := func(ty Type) *Event {
		return &Event{
			TSWall: "2026-08-06T22:14:07.412Z", BoxID: "b", PID: 1, Type: ty,
		}
	}
	for _, ty := range []Type{TypeExec, TypeFile, TypeAPI, TypePolicy, TypeDNS, TypeClose} {
		if err := base(ty).Validate(); err == nil {
			t.Errorf("%s without its sub-object should not validate", ty)
		}
	}
	// Types that carry only the envelope validate without any sub-object.
	for _, ty := range []Type{TypeExit, TypeSyscall} {
		if err := base(ty).Validate(); err != nil {
			t.Errorf("%s should validate with only the envelope: %v", ty, err)
		}
	}
}

// The fixture is the cross-language contract: `src/obs/event.rs` decodes this
// exact file in its own test suite. If the two ever disagree, one of the two
// tests fails rather than the console silently rendering blanks.
func TestGoldenFixtureDecodes(t *testing.T) {
	t.Parallel()

	path := filepath.Join("testdata", "events.jsonl")
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read fixture: %v", err)
	}

	var count int
	for _, line := range splitLines(string(data)) {
		if line == "" {
			continue
		}
		e, err := Decode([]byte(line))
		if err != nil {
			t.Fatalf("fixture line %d does not decode: %v", count+1, err)
		}
		if err := e.Validate(); err != nil {
			t.Errorf("fixture line %d is invalid: %v", count+1, err)
		}
		count++
	}
	if count < len(AllTypes) {
		t.Errorf("fixture has %d events; it should cover all %d types",
			count, len(AllTypes))
	}
}

func splitLines(s string) []string {
	var out []string
	start := 0
	for i := 0; i < len(s); i++ {
		if s[i] == '\n' {
			out = append(out, s[start:i])
			start = i + 1
		}
	}
	if start < len(s) {
		out = append(out, s[start:])
	}
	return out
}

func contains(haystack, needle string) bool {
	for i := 0; i+len(needle) <= len(haystack); i++ {
		if haystack[i:i+len(needle)] == needle {
			return true
		}
	}
	return false
}
