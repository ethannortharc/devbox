// Package transport implements the agent↔collector protocol — §11.3.
//
// Two channels share one connection:
//
//	data    agent → collector, a stream of events
//	control collector → agent, policy updates and capture requests
//
// Both use the same framing: a 4-byte big-endian length followed by that many
// bytes of payload. Length-prefixing rather than newline-delimiting because a
// payload is opaque bytes to the framer — today JSON, tomorrow protobuf
// (ADR-0015) — and a framer that has to know about escaping is a framer that
// will eventually get it wrong.
//
// A handshake precedes the stream so a version-mismatched agent is rejected
// loudly instead of feeding the collector fields it will misread (§7.3).
package transport

import (
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
)

// ProtocolVersion is bumped whenever the event schema or the framing changes.
const ProtocolVersion = 1

// MaxFrameSize bounds a single frame.
//
// A corrupt or hostile length prefix must not make the reader allocate a
// gigabyte. 1 MiB is far above any real event (the largest realistic one is an
// exec with a long argv) and far below anything dangerous.
const MaxFrameSize = 1 << 20

// ErrFrameTooLarge is returned when a length prefix exceeds MaxFrameSize.
var ErrFrameTooLarge = errors.New("transport: frame exceeds the maximum size")

// Hello is the first frame an agent sends. The collector replies with a
// HelloAck, or closes the connection.
type Hello struct {
	Protocol int    `json:"protocol"`
	Version  string `json:"version"`
	BoxID    string `json:"box_id"`
	// Capture lists the domains this agent is capturing, so the console can
	// show what is and is not being watched rather than guessing.
	Capture []string `json:"capture"`
	// Source names the composed capture backends that survived preflight —
	// "ebpf+packet+netfilter", "proc+packet". Domains say what is watched;
	// this says what is doing the watching, and the two answer different
	// questions: proc polling reports `connect` without any process to
	// attribute it to, so a Capture list alone cannot distinguish full
	// coverage from a plausible-looking degraded one.
	//
	// Added after Capture and EBPF, and read with a default on the collector
	// side: an agent predating this field is still a valid agent.
	Source string `json:"source,omitempty"`
	// EBPF is false in the degraded `--no-ebpf` mode (§13), which the UI
	// marks so nobody mistakes proc-polling coverage for kernel coverage.
	EBPF bool `json:"ebpf"`
}

// HelloAck is the collector's reply.
type HelloAck struct {
	Protocol int    `json:"protocol"`
	Accepted bool   `json:"accepted"`
	Reason   string `json:"reason,omitempty"`
}

// WriteFrame writes one length-prefixed frame.
func WriteFrame(w io.Writer, payload []byte) error {
	if len(payload) > MaxFrameSize {
		return fmt.Errorf("%w: %d bytes", ErrFrameTooLarge, len(payload))
	}
	var header [4]byte
	binary.BigEndian.PutUint32(header[:], uint32(len(payload)))
	if _, err := w.Write(header[:]); err != nil {
		return fmt.Errorf("transport: write header: %w", err)
	}
	if _, err := w.Write(payload); err != nil {
		return fmt.Errorf("transport: write payload: %w", err)
	}
	return nil
}

// ReadFrame reads one length-prefixed frame.
//
// Returns io.EOF exactly when the stream ended cleanly on a frame boundary, so
// a caller can distinguish "peer hung up" from "peer hung up mid-frame".
func ReadFrame(r io.Reader) ([]byte, error) {
	var header [4]byte
	if _, err := io.ReadFull(r, header[:]); err != nil {
		if errors.Is(err, io.EOF) {
			return nil, io.EOF
		}
		return nil, fmt.Errorf("transport: read header: %w", err)
	}

	size := binary.BigEndian.Uint32(header[:])
	if size > MaxFrameSize {
		return nil, fmt.Errorf("%w: %d bytes", ErrFrameTooLarge, size)
	}
	if size == 0 {
		return []byte{}, nil
	}

	payload := make([]byte, size)
	if _, err := io.ReadFull(r, payload); err != nil {
		return nil, fmt.Errorf("transport: read payload (%d bytes): %w", size, err)
	}
	return payload, nil
}

// WriteJSON marshals v and writes it as one frame.
func WriteJSON(w io.Writer, v any) error {
	payload, err := json.Marshal(v)
	if err != nil {
		return fmt.Errorf("transport: marshal: %w", err)
	}
	return WriteFrame(w, payload)
}

// ReadJSON reads one frame and unmarshals it into v.
func ReadJSON(r io.Reader, v any) error {
	payload, err := ReadFrame(r)
	if err != nil {
		return err
	}
	if err := json.Unmarshal(payload, v); err != nil {
		return fmt.Errorf("transport: unmarshal: %w", err)
	}
	return nil
}

// ErrRejected is the collector explicitly refusing this agent — a wrong box id,
// a name it does not know. Permanent: retrying cannot change the answer.
var ErrRejected = errors.New("collector rejected the agent")

// ErrProtocol is a version disagreement. Also permanent, and for the same
// reason: neither side will change version by being asked again.
var ErrProtocol = errors.New("protocol mismatch")

// Handshake performs the agent side: send Hello, read HelloAck.
//
// Sentinels rather than message text, because the caller has to act on the
// difference: a refusal must stop the agent, and a connection that died
// mid-handshake must not. Matching on strings would make that distinction a
// property of the wording.
func Handshake(rw io.ReadWriter, hello Hello) error {
	hello.Protocol = ProtocolVersion
	if err := WriteJSON(rw, hello); err != nil {
		return err
	}

	var ack HelloAck
	if err := ReadJSON(rw, &ack); err != nil {
		return fmt.Errorf("transport: no handshake reply: %w", err)
	}
	if !ack.Accepted {
		return fmt.Errorf("transport: %w: %s", ErrRejected, ack.Reason)
	}
	if ack.Protocol != ProtocolVersion {
		return fmt.Errorf(
			"transport: %w — agent speaks %d, collector speaks %d",
			ErrProtocol, ProtocolVersion, ack.Protocol)
	}
	return nil
}
