//go:build linux

package capture

import (
	"context"
	"errors"
	"fmt"
	"io"
	"time"

	"golang.org/x/sys/unix"
)

// CapturePCAP records matching Ethernet frames until the duration or packet
// bound is reached. A valid header-only pcap is returned when the selected
// flow is idle during the window; that is an honest empty capture, not an
// export failure.
//
//nolint:revive // PCAP is the operation and the package also owns other capture sources.
func CapturePCAP(ctx context.Context, filter PCAPFilter, out io.Writer) (int, error) {
	if err := filter.validate(); err != nil {
		return 0, err
	}
	protocol := int((uint16(unix.ETH_P_ALL) << 8) | (uint16(unix.ETH_P_ALL) >> 8))
	fd, err := unix.Socket(
		unix.AF_PACKET,
		unix.SOCK_RAW|unix.SOCK_CLOEXEC|unix.SOCK_NONBLOCK,
		protocol,
	)
	if err != nil {
		return 0, fmt.Errorf("open flow capture (CAP_NET_RAW is required): %w", err)
	}
	// The socket only ever received; a failed close cannot lose captured data.
	defer func() { _ = unix.Close(fd) }()
	if err := writePCAPHeader(out); err != nil {
		return 0, err
	}

	captureCtx, cancel := context.WithTimeout(ctx, filter.Duration)
	defer cancel()
	buffer := make([]byte, 65535)
	poll := []unix.PollFd{{Fd: int32(fd), Events: unix.POLLIN}}
	captured := 0
	for captured < filter.MaxPackets {
		if captureCtx.Err() != nil {
			return captured, nil
		}
		ready, err := unix.Poll(poll, 200)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return captured, fmt.Errorf("poll flow capture: %w", err)
		}
		if ready == 0 {
			continue
		}
		if poll[0].Revents&(unix.POLLERR|unix.POLLHUP|unix.POLLNVAL) != 0 {
			return captured, fmt.Errorf("flow capture stopped (poll events %#x)", poll[0].Revents)
		}
		length, _, err := unix.Recvfrom(fd, buffer, 0)
		if errors.Is(err, unix.EAGAIN) || errors.Is(err, unix.EWOULDBLOCK) {
			continue
		}
		if err != nil {
			return captured, fmt.Errorf("read flow capture: %w", err)
		}
		frame := buffer[:length]
		view, ok := networkPayload(frame)
		if !ok || !filter.matches(view) {
			continue
		}
		if err := writePCAPPacket(out, time.Now(), frame); err != nil {
			return captured, err
		}
		captured++
	}
	return captured, nil
}
