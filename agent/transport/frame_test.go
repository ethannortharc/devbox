package transport

import (
	"bytes"
	"encoding/binary"
	"errors"
	"io"
	"strings"
	"testing"

	"github.com/ethannortharc/devbox/agent/event"
)

func TestFrameRoundTrip(t *testing.T) {
	t.Parallel()

	var buf bytes.Buffer
	payloads := [][]byte{
		[]byte("first"),
		[]byte(""),
		[]byte(strings.Repeat("x", 5000)),
		{0x00, 0xff, 0x0a, 0x0d}, // bytes a line-delimited framer would ruin
	}

	for _, p := range payloads {
		if err := WriteFrame(&buf, p); err != nil {
			t.Fatalf("WriteFrame: %v", err)
		}
	}

	for i, want := range payloads {
		got, err := ReadFrame(&buf)
		if err != nil {
			t.Fatalf("ReadFrame %d: %v", i, err)
		}
		if !bytes.Equal(got, want) {
			t.Errorf("frame %d = %q, want %q", i, got, want)
		}
	}

	if _, err := ReadFrame(&buf); !errors.Is(err, io.EOF) {
		t.Errorf("a drained stream should report EOF, got %v", err)
	}
}

func TestOversizedFrameIsRefusedOnWrite(t *testing.T) {
	t.Parallel()

	var buf bytes.Buffer
	err := WriteFrame(&buf, make([]byte, MaxFrameSize+1))
	if !errors.Is(err, ErrFrameTooLarge) {
		t.Errorf("expected ErrFrameTooLarge, got %v", err)
	}
	if buf.Len() != 0 {
		t.Error("nothing should have been written")
	}
}

func TestOversizedLengthPrefixIsRefusedOnRead(t *testing.T) {
	t.Parallel()

	// A hostile or corrupt prefix must not make the reader allocate.
	var header [4]byte
	binary.BigEndian.PutUint32(header[:], 0xFFFFFFFF)
	r := bytes.NewReader(header[:])

	_, err := ReadFrame(r)
	if !errors.Is(err, ErrFrameTooLarge) {
		t.Errorf("expected ErrFrameTooLarge, got %v", err)
	}
}

func TestTruncatedFrameIsAnError(t *testing.T) {
	t.Parallel()

	var buf bytes.Buffer
	if err := WriteFrame(&buf, []byte("hello world")); err != nil {
		t.Fatal(err)
	}
	truncated := buf.Bytes()[:6] // header plus two payload bytes

	_, err := ReadFrame(bytes.NewReader(truncated))
	if err == nil {
		t.Fatal("a truncated frame must be an error")
	}
	if errors.Is(err, io.EOF) {
		t.Error("mid-frame truncation must not look like a clean EOF")
	}
}

func TestJSONFramesCarryEvents(t *testing.T) {
	t.Parallel()

	var buf bytes.Buffer
	original := &event.Event{
		TSWall: "2026-08-06T22:14:07.412Z", TSMonoNS: 1, BoxID: "b", PID: 7,
		Type: event.TypeExec,
		Exec: &event.Exec{Path: "/bin/sh", Argv: []string{"sh", "-c", "echo hi"}},
	}
	if err := WriteJSON(&buf, original); err != nil {
		t.Fatalf("WriteJSON: %v", err)
	}

	var got event.Event
	if err := ReadJSON(&buf, &got); err != nil {
		t.Fatalf("ReadJSON: %v", err)
	}
	if got.Exec == nil || got.Exec.Path != "/bin/sh" {
		t.Errorf("event did not survive framing: %+v", got)
	}
	if len(got.Exec.Argv) != 3 {
		t.Errorf("argv did not survive framing: %v", got.Exec.Argv)
	}
}

// pipe pairs two buffers so a handshake can be driven without a socket.
type pipe struct {
	in  *bytes.Buffer
	out *bytes.Buffer
}

func (p pipe) Read(b []byte) (int, error)  { return p.in.Read(b) }
func (p pipe) Write(b []byte) (int, error) { return p.out.Write(b) }

func TestHandshakeSucceeds(t *testing.T) {
	t.Parallel()

	var toAgent, fromAgent bytes.Buffer
	if err := WriteJSON(&toAgent, HelloAck{Protocol: ProtocolVersion, Accepted: true}); err != nil {
		t.Fatal(err)
	}

	conn := pipe{in: &toAgent, out: &fromAgent}
	err := Handshake(conn, Hello{
		Version: "0.1.3", BoxID: "myapp",
		Capture: []string{"exec", "connect"}, EBPF: true,
	})
	if err != nil {
		t.Fatalf("Handshake: %v", err)
	}

	var hello Hello
	if err := ReadJSON(&fromAgent, &hello); err != nil {
		t.Fatalf("collector could not read the Hello: %v", err)
	}
	if hello.Protocol != ProtocolVersion {
		t.Errorf("Hello.Protocol = %d, want %d", hello.Protocol, ProtocolVersion)
	}
	if hello.BoxID != "myapp" || !hello.EBPF {
		t.Errorf("Hello lost fields: %+v", hello)
	}
}

func TestHandshakeRejectsAVersionMismatch(t *testing.T) {
	t.Parallel()

	var toAgent, fromAgent bytes.Buffer
	if err := WriteJSON(&toAgent, HelloAck{
		Protocol: ProtocolVersion + 1, Accepted: true,
	}); err != nil {
		t.Fatal(err)
	}

	err := Handshake(pipe{in: &toAgent, out: &fromAgent}, Hello{BoxID: "b"})
	if err == nil || !strings.Contains(err.Error(), "protocol mismatch") {
		t.Errorf("expected a protocol mismatch, got %v", err)
	}
}

func TestHandshakeSurfacesARejection(t *testing.T) {
	t.Parallel()

	var toAgent, fromAgent bytes.Buffer
	if err := WriteJSON(&toAgent, HelloAck{
		Protocol: ProtocolVersion, Accepted: false, Reason: "agent build does not match host",
	}); err != nil {
		t.Fatal(err)
	}

	err := Handshake(pipe{in: &toAgent, out: &fromAgent}, Hello{BoxID: "b"})
	if err == nil || !strings.Contains(err.Error(), "agent build does not match host") {
		t.Errorf("the rejection reason must reach the caller, got %v", err)
	}
}

func TestHandshakeFailsWhenTheCollectorSaysNothing(t *testing.T) {
	t.Parallel()

	var empty, fromAgent bytes.Buffer
	err := Handshake(pipe{in: &empty, out: &fromAgent}, Hello{BoxID: "b"})
	if err == nil || !strings.Contains(err.Error(), "no handshake reply") {
		t.Errorf("expected a handshake-reply error, got %v", err)
	}
}

// TestHandshakeErrorsAreDistinguishable is what lets the agent tell an outage
// from a refusal.
//
// The agent must exit on a refusal and must *not* exit on a connection that
// died mid-exchange: exiting there hands the supervisor a restart, and the
// restart reloads the ruleset, clearing every allow set DNS had filled. Matching
// on message text would make that distinction a property of the wording.
func TestHandshakeErrorsAreDistinguishable(t *testing.T) {
	t.Parallel()

	t.Run("a refusal is permanent", func(t *testing.T) {
		rw := &fakeConn{reply: HelloAck{Accepted: false, Reason: "unknown box"}}
		err := Handshake(rw, Hello{BoxID: "nope"})
		if !errors.Is(err, ErrRejected) {
			t.Errorf("err = %v, want ErrRejected", err)
		}
		if errors.Is(err, ErrProtocol) {
			t.Error("a refusal must not also read as a protocol mismatch")
		}
	})

	t.Run("a version disagreement is permanent", func(t *testing.T) {
		rw := &fakeConn{reply: HelloAck{Accepted: true, Protocol: ProtocolVersion + 1}}
		err := Handshake(rw, Hello{BoxID: "b"})
		if !errors.Is(err, ErrProtocol) {
			t.Errorf("err = %v, want ErrProtocol", err)
		}
	})

	t.Run("a dropped connection is neither", func(t *testing.T) {
		// The collector accepted and then went away before replying, which is
		// what a restart looks like from here.
		rw := &fakeConn{truncate: true}
		err := Handshake(rw, Hello{BoxID: "b"})
		if err == nil {
			t.Fatal("expected an error")
		}
		if errors.Is(err, ErrRejected) || errors.Is(err, ErrProtocol) {
			t.Errorf("a dropped handshake must be retryable, got %v", err)
		}
	})
}

// fakeConn answers one handshake, or hangs up part-way through.
type fakeConn struct {
	reply    HelloAck
	truncate bool
	out      bytes.Buffer
	in       *bytes.Reader
}

func (f *fakeConn) Write(p []byte) (int, error) { return f.out.Write(p) }

func (f *fakeConn) Read(p []byte) (int, error) {
	if f.in == nil {
		if f.truncate {
			f.in = bytes.NewReader(nil) // EOF: accepted, then gone
		} else {
			var buf bytes.Buffer
			if err := WriteJSON(&buf, f.reply); err != nil {
				return 0, err
			}
			f.in = bytes.NewReader(buf.Bytes())
		}
	}
	return f.in.Read(p)
}
