//go:build linux

package capture

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"net/netip"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
	"golang.org/x/sys/unix"
)

type preparedPacket struct {
	*Packet
	fd       int
	close    sync.Once
	closeErr error
}

// NewPacket opens the host network namespace's packet tap immediately. Like
// NewEBPF, this is a preflight: the handshake must not advertise DNS/TLS and a
// domain firewall must not be restored unless the required capability works.
func NewPacket(boxID string, boot time.Time) (Source, error) {
	resolvers, err := readResolvers("/etc/resolv.conf")
	if err != nil {
		return nil, err
	}
	protocol := int((uint16(unix.ETH_P_ALL) << 8) | (uint16(unix.ETH_P_ALL) >> 8))
	fd, err := unix.Socket(
		unix.AF_PACKET,
		unix.SOCK_RAW|unix.SOCK_CLOEXEC|unix.SOCK_NONBLOCK,
		protocol,
	)
	if err != nil {
		return nil, fmt.Errorf("open packet capture (CAP_NET_RAW is required): %w", err)
	}
	return &preparedPacket{
		Packet: &Packet{BoxID: boxID, Boot: boot, Resolvers: resolvers},
		fd:     fd,
	}, nil
}

func readResolvers(path string) (map[netip.Addr]struct{}, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("read DNS resolvers from %s: %w", path, err)
	}
	// Read-only; a failed close has nothing to tell the caller.
	defer func() { _ = file.Close() }()

	resolvers := make(map[netip.Addr]struct{})
	scanner := bufio.NewScanner(file)
	for scanner.Scan() {
		fields := strings.Fields(strings.SplitN(scanner.Text(), "#", 2)[0])
		if len(fields) < 2 || fields[0] != "nameserver" {
			continue
		}
		if address, parseErr := netip.ParseAddr(fields[1]); parseErr == nil {
			resolvers[address.Unmap()] = struct{}{}
		}
	}
	if err := scanner.Err(); err != nil {
		return nil, fmt.Errorf("read DNS resolvers from %s: %w", path, err)
	}
	if len(resolvers) == 0 {
		return nil, fmt.Errorf("no nameserver found in %s", path)
	}
	return resolvers, nil
}

func (p *preparedPacket) Run(ctx context.Context, out chan<- *event.Event) error {
	buffer := make([]byte, 65536)
	poll := []unix.PollFd{{Fd: int32(p.fd), Events: unix.POLLIN}}
	for {
		if err := ctx.Err(); err != nil {
			return nil
		}
		ready, err := unix.Poll(poll, 200)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return fmt.Errorf("poll packet capture: %w", err)
		}
		if ready == 0 {
			continue
		}
		if poll[0].Revents&(unix.POLLERR|unix.POLLHUP|unix.POLLNVAL) != 0 {
			return fmt.Errorf("packet capture stopped (poll events %#x)", poll[0].Revents)
		}
		length, _, err := unix.Recvfrom(p.fd, buffer, 0)
		if errors.Is(err, unix.EAGAIN) || errors.Is(err, unix.EWOULDBLOCK) {
			continue
		}
		if err != nil {
			return fmt.Errorf("read packet capture: %w", err)
		}
		if decoded := p.decodePacket(buffer[:length], time.Now()); decoded != nil {
			if err := Send(ctx, out, decoded); err != nil {
				return err
			}
		}
	}
}

func (p *preparedPacket) Close() error {
	p.close.Do(func() { p.closeErr = unix.Close(p.fd) })
	return p.closeErr
}
