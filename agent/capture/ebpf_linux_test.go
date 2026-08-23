//go:build linux && ebpf

package capture

import (
	"context"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

// TestEBPFLoadsAndCaptures is the privileged Linux gate. Decoder fixtures
// prove byte layouts on every platform; this test proves the generated object
// actually loads, attaches, and produces records from the running kernel.
func TestEBPFLoadsAndCaptures(t *testing.T) {
	if os.Geteuid() != 0 {
		t.Skip("loading eBPF probes requires root or equivalent capabilities")
	}
	if _, err := os.Stat("/sys/kernel/btf/vmlinux"); err != nil {
		t.Skip("the running kernel has no readable BTF")
	}

	source, err := NewEBPF("kernel-gate", bootWall(t))
	if err != nil {
		t.Fatalf("construct eBPF source: %v", err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	events := make(chan *event.Event, 4096)
	done := make(chan error, 1)
	go func() { done <- source.Run(ctx, events) }()

	// Give the loader time to attach before generating the three signals.
	select {
	case runErr := <-done:
		t.Fatalf("eBPF source stopped while attaching: %v", runErr)
	case <-time.After(250 * time.Millisecond):
	}

	if err := exec.Command("/bin/true").Run(); err != nil {
		t.Fatalf("run workload exec: %v", err)
	}
	path := filepath.Join(t.TempDir(), "probe-openat")
	if err := os.WriteFile(path, []byte("captured"), 0o600); err != nil {
		t.Fatalf("run workload file write: %v", err)
	}

	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen for workload connection: %v", err)
	}
	accepted := make(chan struct{})
	go func() {
		conn, acceptErr := listener.Accept()
		if acceptErr == nil {
			_ = conn.Close()
		}
		close(accepted)
	}()
	conn, err := net.Dial("tcp", listener.Addr().String())
	if err != nil {
		_ = listener.Close()
		t.Fatalf("run workload connection: %v", err)
	}
	_ = conn.Close()
	<-accepted
	_ = listener.Close()

	want := map[event.Type]bool{
		event.TypeExec:    false,
		event.TypeConnect: false,
		event.TypeFile:    false,
	}
	deadline := time.NewTimer(10 * time.Second)
	defer deadline.Stop()
	for {
		select {
		case captured := <-events:
			if _, tracked := want[captured.Type]; tracked {
				want[captured.Type] = true
			}
			if want[event.TypeExec] && want[event.TypeConnect] && want[event.TypeFile] {
				cancel()
				if runErr := <-done; runErr != nil {
					t.Fatalf("eBPF source did not stop cleanly: %v", runErr)
				}
				return
			}
		case runErr := <-done:
			t.Fatalf("eBPF source stopped before the workload was captured: %v", runErr)
		case <-deadline.C:
			t.Fatalf("timed out waiting for kernel events: %+v", want)
		}
	}
}

func bootWall(t *testing.T) time.Time {
	t.Helper()
	raw, err := os.ReadFile("/proc/uptime")
	if err != nil {
		t.Fatalf("read kernel uptime: %v", err)
	}
	var seconds float64
	if _, err := fmt.Sscanf(string(raw), "%f", &seconds); err != nil {
		t.Fatalf("parse kernel uptime: %v", err)
	}
	return time.Now().Add(-time.Duration(seconds * float64(time.Second)))
}
