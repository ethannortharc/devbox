package main

import (
	"errors"
	"fmt"
	"net"
	"os"
	"time"

	"golang.org/x/sys/unix"
)

// stdioConn carries the ordinary framed agent protocol through a runtime's
// existing authenticated exec channel. It is the VM transport when a Unix
// socket inode cannot cross 9p/virtiofs (Lima, Multipass, Incus VMs).
type stdioConn struct {
	in               *os.File
	out              *os.File
	own              bool
	restoreTerminals []func() error
}

func newStdioConn() (net.Conn, error) {
	// Inherited descriptors are commonly constructed in blocking mode, and an
	// os.File decides whether deadlines are supported when it is created.
	// Re-create duplicated non-blocking descriptors so Go's poller can enforce
	// the liveness bounds stream() relies on.
	in, err := duplicatePollable(os.Stdin)
	if err != nil {
		return nil, fmt.Errorf("prepare stdin for deadlines: %w", err)
	}
	out, err := duplicatePollable(os.Stdout)
	if err != nil {
		_ = in.Close()
		return nil, fmt.Errorf("prepare stdout for deadlines: %w", err)
	}
	restoreIn, err := makeRawIfTerminal(in)
	if err != nil {
		_ = in.Close()
		_ = out.Close()
		return nil, fmt.Errorf("make stdin framing-safe: %w", err)
	}
	restoreOut, err := makeRawIfTerminal(out)
	if err != nil {
		if restoreIn != nil {
			_ = restoreIn()
		}
		_ = in.Close()
		_ = out.Close()
		return nil, fmt.Errorf("make stdout framing-safe: %w", err)
	}
	restores := make([]func() error, 0, 2)
	if restoreIn != nil {
		restores = append(restores, restoreIn)
	}
	if restoreOut != nil {
		restores = append(restores, restoreOut)
	}
	return &stdioConn{in: in, out: out, own: true, restoreTerminals: restores}, nil
}

func newStdioFileConn(in, out *os.File) net.Conn { return &stdioConn{in: in, out: out} }

func duplicatePollable(file *os.File) (*os.File, error) {
	fd, err := unix.Dup(int(file.Fd()))
	if err != nil {
		return nil, err
	}
	if err := unix.SetNonblock(fd, true); err != nil {
		_ = unix.Close(fd)
		return nil, err
	}
	duplicate := os.NewFile(uintptr(fd), file.Name()+"-devbox-pollable")
	if duplicate == nil {
		_ = unix.Close(fd)
		return nil, errors.New("os.NewFile refused the duplicated descriptor")
	}
	return duplicate, nil
}

func (c *stdioConn) Read(payload []byte) (int, error)  { return c.in.Read(payload) }
func (c *stdioConn) Write(payload []byte) (int, error) { return c.out.Write(payload) }
func (c *stdioConn) Close() error {
	if !c.own {
		return nil
	}
	var errs []error
	// Restore in reverse order. stdin and stdout commonly name the same sudo
	// pty, so the second MakeRaw observed the already-raw state and must be
	// unwound before the first restores the original line discipline.
	for i := len(c.restoreTerminals) - 1; i >= 0; i-- {
		errs = append(errs, c.restoreTerminals[i]())
	}
	errs = append(errs, c.in.Close(), c.out.Close())
	return errors.Join(errs...)
}
func (*stdioConn) LocalAddr() net.Addr  { return stdioAddr("agent") }
func (*stdioConn) RemoteAddr() net.Addr { return stdioAddr("collector") }
func (c *stdioConn) SetDeadline(deadline time.Time) error {
	readErr := c.in.SetReadDeadline(deadline)
	writeErr := c.out.SetWriteDeadline(deadline)
	return errors.Join(readErr, writeErr)
}
func (c *stdioConn) SetReadDeadline(deadline time.Time) error {
	return c.in.SetReadDeadline(deadline)
}
func (c *stdioConn) SetWriteDeadline(deadline time.Time) error {
	return c.out.SetWriteDeadline(deadline)
}

type stdioAddr string

func (stdioAddr) Network() string  { return "stdio" }
func (a stdioAddr) String() string { return string(a) }

// makeRawIfTerminal neutralises sudo's use_pty line discipline.
//
// Length-prefixed JSON is binary framing: ONLCR, ICRNL, canonical reads and
// control characters all corrupt it. Runtime exec without sudo supplies pipes,
// for which ENOTTY is the expected no-op.
func makeRawIfTerminal(file *os.File) (func() error, error) {
	// File.Fd() would silently put this descriptor back into blocking mode and
	// disable every deadline on stdioConn. Control exposes the fd without
	// detaching it from Go's poller.
	rawConn, err := file.SyscallConn()
	if err != nil {
		return nil, err
	}
	var original *unix.Termios
	var operationErr error
	controlErr := rawConn.Control(func(fd uintptr) {
		original, operationErr = unix.IoctlGetTermios(int(fd), ioctlGetTermios)
		if errors.Is(operationErr, unix.ENOTTY) {
			original = nil
			operationErr = nil
			return
		}
		if operationErr != nil {
			return
		}
		raw := rawTermios(*original)
		operationErr = unix.IoctlSetTermios(int(fd), ioctlSetTermios, &raw)
	})
	if err := errors.Join(controlErr, operationErr); err != nil {
		return nil, err
	}
	if original == nil {
		return nil, nil
	}
	return func() error {
		var restoreErr error
		controlErr := rawConn.Control(func(fd uintptr) {
			restoreErr = unix.IoctlSetTermios(int(fd), ioctlSetTermios, original)
		})
		return errors.Join(controlErr, restoreErr)
	}, nil
}

func rawTermios(term unix.Termios) unix.Termios {
	term.Iflag &^= unix.IGNBRK | unix.BRKINT | unix.PARMRK | unix.ISTRIP |
		unix.INLCR | unix.IGNCR | unix.ICRNL | unix.IXON
	term.Oflag &^= unix.OPOST
	term.Lflag &^= unix.ECHO | unix.ECHONL | unix.ICANON | unix.ISIG | unix.IEXTEN
	term.Cflag &^= unix.CSIZE | unix.PARENB
	term.Cflag |= unix.CS8
	term.Cc[unix.VMIN] = 1
	term.Cc[unix.VTIME] = 0
	return term
}
