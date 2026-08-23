//go:build linux && ebpf

package bpf

import (
	"errors"
	"fmt"

	"github.com/cilium/ebpf/link"
	"github.com/cilium/ebpf/ringbuf"
	"github.com/cilium/ebpf/rlimit"
)

// Runtime is one loaded copy of the devbox probes and their ring-buffer
// readers. Closing it detaches every probe before closing the maps.
type Runtime struct {
	Objects DevboxObjects
	Exec    *ringbuf.Reader
	Net     *ringbuf.Reader
	File    *ringbuf.Reader
	links   []link.Link
}

// Load loads the generated CO-RE object, enables the trace-all sentinel, and
// attaches every program to the hook named in devbox.bpf.c.
func Load() (_ *Runtime, err error) {
	if err := rlimit.RemoveMemlock(); err != nil {
		return nil, fmt.Errorf("raise the BPF memlock limit: %w", err)
	}

	var objects DevboxObjects
	if err := LoadDevboxObjects(&objects, nil); err != nil {
		return nil, fmt.Errorf("load devbox BPF objects: %w", err)
	}
	runtime := &Runtime{Objects: objects}
	defer func() {
		if err != nil {
			_ = runtime.Close()
		}
	}()

	// Key zero is the explicit trace-all sentinel. Without it the program's
	// default-deny cgroup filter drops every record, so a loader that forgets
	// this succeeds while producing a perfectly quiet, false timeline.
	var key uint64
	value := uint8(1)
	if err := runtime.Objects.TracedCgroups.Put(key, value); err != nil {
		return nil, fmt.Errorf("enable the trace-all cgroup sentinel: %w", err)
	}

	attachments := []struct {
		name   string
		attach func() (link.Link, error)
	}{
		{"sched/sched_process_exec", func() (link.Link, error) {
			return link.Tracepoint("sched", "sched_process_exec", runtime.Objects.HandleExec, nil)
		}},
		{"tcp_v4_connect", func() (link.Link, error) {
			return link.Kprobe("tcp_v4_connect", runtime.Objects.HandleTcpV4Connect, nil)
		}},
		{"tcp_v6_connect", func() (link.Link, error) {
			return link.Kprobe("tcp_v6_connect", runtime.Objects.HandleTcpV6Connect, nil)
		}},
		{"tcp_finish_connect", func() (link.Link, error) {
			return link.Kprobe("tcp_finish_connect", runtime.Objects.HandleTcpFinishConnect, nil)
		}},
		{"inet_csk_accept", func() (link.Link, error) {
			return link.Kretprobe("inet_csk_accept", runtime.Objects.HandleAccept, nil)
		}},
		{"syscalls/sys_enter_openat", func() (link.Link, error) {
			return link.Tracepoint("syscalls", "sys_enter_openat", runtime.Objects.HandleOpenat, nil)
		}},
	}
	for _, attachment := range attachments {
		attached, attachErr := attachment.attach()
		if attachErr != nil {
			return nil, fmt.Errorf("attach %s: %w", attachment.name, attachErr)
		}
		runtime.links = append(runtime.links, attached)
	}

	if runtime.Exec, err = ringbuf.NewReader(runtime.Objects.ExecEvents); err != nil {
		return nil, fmt.Errorf("open exec ring buffer: %w", err)
	}
	if runtime.Net, err = ringbuf.NewReader(runtime.Objects.NetEvents); err != nil {
		return nil, fmt.Errorf("open network ring buffer: %w", err)
	}
	if runtime.File, err = ringbuf.NewReader(runtime.Objects.FileEvents); err != nil {
		return nil, fmt.Errorf("open file ring buffer: %w", err)
	}
	return runtime, nil
}

// Close unblocks readers, detaches probes, and releases maps. It is
// idempotent enough for both the error path and normal cancellation.
func (r *Runtime) Close() error {
	var errs []error
	for _, reader := range []*ringbuf.Reader{r.Exec, r.Net, r.File} {
		if reader != nil {
			errs = append(errs, reader.Close())
		}
	}
	for i := len(r.links) - 1; i >= 0; i-- {
		errs = append(errs, r.links[i].Close())
	}
	errs = append(errs, r.Objects.Close())
	return errors.Join(errs...)
}
