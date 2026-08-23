package main

import (
	"errors"
	"os"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

func TestRawTermiosProtectsBinaryFraming(t *testing.T) {
	t.Parallel()

	term := unix.Termios{
		Iflag: unix.IGNBRK | unix.BRKINT | unix.PARMRK | unix.ISTRIP |
			unix.INLCR | unix.IGNCR | unix.ICRNL | unix.IXON,
		Oflag: unix.OPOST,
		Lflag: unix.ECHO | unix.ECHONL | unix.ICANON | unix.ISIG | unix.IEXTEN,
		Cflag: unix.CSIZE | unix.PARENB,
	}
	raw := rawTermios(term)
	if raw.Iflag != 0 || raw.Oflag&unix.OPOST != 0 || raw.Lflag != 0 {
		t.Fatalf("line discipline still rewrites frames: %+v", raw)
	}
	if raw.Cflag&unix.CSIZE != unix.CS8 || raw.Cflag&unix.PARENB != 0 {
		t.Fatalf("terminal is not 8-bit clean: cflag=%#x", raw.Cflag)
	}
	if raw.Cc[unix.VMIN] != 1 || raw.Cc[unix.VTIME] != 0 {
		t.Fatalf("raw read controls = VMIN %d VTIME %d", raw.Cc[unix.VMIN], raw.Cc[unix.VTIME])
	}
}

func TestStdioTransportEnforcesPipeDeadlines(t *testing.T) {
	t.Parallel()

	agentIn, hostOut, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	hostIn, agentOut, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer agentIn.Close()
	defer hostOut.Close()
	defer hostIn.Close()
	defer agentOut.Close()

	conn := newStdioFileConn(agentIn, agentOut)
	if err := conn.SetReadDeadline(time.Now().Add(20 * time.Millisecond)); err != nil {
		t.Fatalf("set read deadline on exec pipe: %v", err)
	}
	_, err = conn.Read(make([]byte, 1))
	if !errors.Is(err, os.ErrDeadlineExceeded) {
		t.Fatalf("blocked stdio read ignored its deadline: %v", err)
	}
	if err := conn.SetWriteDeadline(time.Now().Add(time.Second)); err != nil {
		t.Fatalf("set write deadline on exec pipe: %v", err)
	}
	if err := conn.SetDeadline(time.Time{}); err != nil {
		t.Fatalf("clear stdio deadlines: %v", err)
	}
}

func TestProductionStdioConnKeepsPollerDeadlines(t *testing.T) {
	// newStdioConn intentionally reads the process globals. Keep this test
	// sequential so no parallel test observes the temporary pipe endpoints.
	agentIn, hostOut, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	hostIn, agentOut, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer agentIn.Close()
	defer hostOut.Close()
	defer hostIn.Close()
	defer agentOut.Close()

	originalIn, originalOut := os.Stdin, os.Stdout
	os.Stdin, os.Stdout = agentIn, agentOut
	conn, err := newStdioConn()
	os.Stdin, os.Stdout = originalIn, originalOut
	if err != nil {
		t.Fatalf("newStdioConn: %v", err)
	}
	defer conn.Close()

	if err := conn.SetReadDeadline(time.Now().Add(20 * time.Millisecond)); err != nil {
		t.Fatalf("set production read deadline: %v", err)
	}
	_, err = conn.Read(make([]byte, 1))
	if !errors.Is(err, os.ErrDeadlineExceeded) {
		t.Fatalf("production stdio read ignored its deadline: %v", err)
	}
}

func TestStdioTransportDoesNotClaimUnsupportedDeadlines(t *testing.T) {
	t.Parallel()

	regular, err := os.CreateTemp(t.TempDir(), "regular-file")
	if err != nil {
		t.Fatal(err)
	}
	defer regular.Close()

	conn := newStdioFileConn(regular, regular)
	if err := conn.SetDeadline(time.Now().Add(time.Second)); err == nil {
		t.Fatal("regular files do not support deadlines; success would be a false guarantee")
	}
}
