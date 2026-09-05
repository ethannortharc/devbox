package capture

import (
	"context"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"

	"github.com/ethannortharc/devbox/agent/event"
)

// DefaultFileScope is what a file event has to be under to be worth exporting.
//
// §7.1 promises "File: open/create/write under the workspace", and the eBPF
// probe cannot keep that promise on its own: `sys_enter_openat` fires for every
// process in the traced cgroup, so an idle box produced thousands of events a
// minute for /dev/null, /etc/passwd, /nix/store and journald's stream sockets.
// Eight hours of that grew one box's store to 258 MB and pushed `behavior
// summary` past its scan limit, so the default window covered seven minutes of
// systemd noise instead of the session anyone wanted to read.
const DefaultFileScope = "/workspace"

// ParseFileScope reads a comma-separated prefix list.
//
// Absolute paths only, and a relative entry is an error rather than a silently
// dropped one: an unusable list would leave the scope empty, which admits
// everything — so a typo would quietly restore the flood this exists to stop.
func ParseFileScope(spec string) ([]string, error) {
	var prefixes []string
	seen := make(map[string]bool)
	for _, raw := range strings.Split(spec, ",") {
		trimmed := strings.TrimSpace(raw)
		if trimmed == "" {
			continue
		}
		if !strings.HasPrefix(trimmed, "/") {
			return nil, fmt.Errorf("file scope %q is not an absolute path", trimmed)
		}
		cleaned := path.Clean(trimmed)
		if seen[cleaned] {
			continue
		}
		seen[cleaned] = true
		prefixes = append(prefixes, cleaned)
	}
	return prefixes, nil
}

// maxKernelThreads bounds the pids remembered for exit suppression.
//
// A box has a few hundred kernel threads and they rarely exit, so this is far
// above the working set. Refusing to grow past it means the worst case is a
// handful of unfiltered exit events, not an agent whose memory tracks pid
// churn.
const maxKernelThreads = 1024

// scopeBuffer decouples the filter from the source it wraps.
//
// Small on purpose: a full outbound channel is backpressure, and that is the
// behaviour §7.3 asks for. A large buffer here would only move the queue.
const scopeBuffer = 64

// Scope narrows a source's events to what a reader asked to watch.
//
// It sits between capture and the transport, so an out-of-scope event never
// reaches the pending queue and never counts as dropped: dropped means "lost
// because we could not keep up", and these were never wanted. They are counted
// separately instead, because a filter nobody can see is indistinguishable from
// a probe that stopped firing.
//
// Two rules, both about noise that is not evidence of anything:
//
//	file  a path outside every configured prefix
//	exec  a kernel thread — no argv, and no /proc/<pid>/exe behind it
type Scope struct {
	// Inner is the source being narrowed. Name, Domains and Close all
	// delegate, so the handshake and the status file keep describing the
	// capture that is actually running.
	Inner Source

	prefixes []string
	// procRoot is /proc, overridden in tests. The kernel-thread rule is the
	// only part of this that has to ask the kernel anything.
	procRoot string

	fileOutOfScope atomic.Uint64
	kernelNoise    atomic.Uint64
	argvRedacted   atomic.Uint64

	mu       sync.Mutex
	kthreads map[uint32]struct{}
}

// NewScope narrows source to the given prefixes. An empty list admits
// everything, which is the documented escape hatch for "show me every path".
func NewScope(source Source, prefixes []string) *Scope {
	return &Scope{
		Inner:    source,
		prefixes: prefixes,
		procRoot: "/proc",
		kthreads: make(map[uint32]struct{}),
	}
}

// Prefixes reports the configured scope, for the handshake and the status file.
func (s *Scope) Prefixes() []string { return s.prefixes }

// FileOutOfScope counts file events suppressed for being outside the scope.
func (s *Scope) FileOutOfScope() uint64 { return s.fileOutOfScope.Load() }

// KernelNoise counts exec and exit events suppressed as kernel threads.
func (s *Scope) KernelNoise() uint64 { return s.kernelNoise.Load() }

// ArgvRedacted counts exec events that had a credential removed from their
// argv. Published rather than merely applied, for the same reason the other
// two are: a filter nobody can see is indistinguishable from one that stopped
// running, and this one is the difference between a report that can be handed
// to someone and a report that cannot.

// ArgvRedacted is how many exec argv words were replaced with `***` before
// the event left the agent.
func (s *Scope) ArgvRedacted() uint64 { return s.argvRedacted.Load() }

// Name implements Source, naming the source underneath rather than the filter.
func (s *Scope) Name() string { return s.Inner.Name() }

// Domains implements Source. Narrowing a domain is not losing it: file events
// still arrive, from under the workspace.
func (s *Scope) Domains() []event.Type { return s.Inner.Domains() }

// Close releases the wrapped source's privileged handles.
func (s *Scope) Close() error { return Close(s.Inner) }

// Run streams the inner source, forwarding only what the scope admits.
func (s *Scope) Run(ctx context.Context, out chan<- *event.Event) error {
	// Its own cancellable context so a failed forward stops the source rather
	// than leaving it writing into a channel nobody reads.
	innerCtx, cancel := context.WithCancel(ctx)
	defer cancel()

	inner := make(chan *event.Event, scopeBuffer)
	done := make(chan error, 1)
	go func() {
		err := s.Inner.Run(innerCtx, inner)
		close(inner)
		done <- err
	}()

	var forwardErr error
	for e := range inner {
		// Kept draining after a failed forward. Returning here would leave the
		// inner source blocked on a send forever, and Run must not return
		// while a goroutine can still write to the caller's channel.
		if forwardErr != nil {
			continue
		}
		if !s.admit(e) {
			continue
		}
		s.redact(e)
		if err := Send(ctx, out, e); err != nil {
			forwardErr = err
			cancel()
		}
	}
	innerErr := <-done

	// A cancellation this function performed is not a failure to report: the
	// caller asked us to stop, exactly as the other sources treat it.
	if ctx.Err() != nil {
		return nil
	}
	if forwardErr != nil {
		return forwardErr
	}
	return innerErr
}

// redact removes credentials from an exec event's argv.
//
// Here rather than in either source, because both of them produce execs and a
// rule enforced in one place is a rule. Applied to what is *about to be sent*,
// so nothing that skipped the filter can reach the transport by another route.
//
// Deliberately after admit: an event nobody will send needs nothing done to
// it, and exec is the only type whose argv exists.
func (s *Scope) redact(e *event.Event) {
	if e.Type != event.TypeExec || e.Exec == nil {
		return
	}
	if RedactArgv(e.Exec.Argv) {
		s.argvRedacted.Add(1)
	}
}

// admit decides one event, counting whatever it suppresses.
func (s *Scope) admit(e *event.Event) bool {
	switch e.Type {
	case event.TypeFile:
		if s.inScope(e) {
			return true
		}
		s.fileOutOfScope.Add(1)
		return false
	case event.TypeExec:
		if !s.kernelThread(e) {
			// A pid can be reused by a real process, so an admitted exec
			// clears whatever the previous occupant left behind.
			s.forget(e.PID)
			return true
		}
		s.remember(e.PID)
		s.kernelNoise.Add(1)
		return false
	case event.TypeExit:
		// An exit carries no sub-object, so it cannot be judged on its own —
		// and by the time it arrives the process is gone, so /proc cannot
		// answer either. What is left is the exec that introduced the pid.
		if !s.isKernelThread(e.PID) {
			return true
		}
		s.forget(e.PID)
		s.kernelNoise.Add(1)
		return false
	default:
		return true
	}
}

// inScope reports whether a file event's path is under a configured prefix.
//
// Paths are cleaned first, so `/workspace/../etc/shadow` is judged as
// `/etc/shadow` — the prefix names a subtree, not a string the path starts
// with. `/workspace` therefore does not admit `/workspaces`.
//
// A relative path is never in scope. The openat tracepoint records the
// pathname argument and not the directory fd it is resolved against, so a bare
// `lib` or `..` — 16% of the events on a real box, all of them path-walk
// fragments from systemd and nss — cannot be resolved to a subtree here
// without guessing. Guessing wrongly would be worse than dropping.
func (s *Scope) inScope(e *event.Event) bool {
	if len(s.prefixes) == 0 {
		return true
	}
	if e.File == nil {
		// Nothing to judge. The collector will reject it for the missing
		// sub-object; suppressing it here would hide a decode fault as a
		// scope decision.
		return true
	}
	cleaned := path.Clean(e.File.Path)
	if !strings.HasPrefix(cleaned, "/") {
		return false
	}
	for _, prefix := range s.prefixes {
		if prefix == "/" {
			return true
		}
		if cleaned == prefix || strings.HasPrefix(cleaned, prefix+"/") {
			return true
		}
	}
	return false
}

// kernelThread reports whether an exec event describes a kernel thread.
//
// The /proc lookup is guarded by the argv test because it is the expensive
// half: every real exec carries an argv, so the syscall runs only for the rare
// event that has none.
//
// Only a *missing* exe means kernel thread. A permission error means this
// agent could not look, and suppressing an exec on that basis would hide real
// processes from an unprivileged agent — the one situation where the evidence
// matters most.
//
// The link is followed, not merely stat-ed. A kernel thread has a
// `/proc/<pid>/exe` symlink like everything else; what it does not have is
// anything on the other end. Checking the link itself passed every kernel
// thread on a live box straight through, which is how this was found.
func (s *Scope) kernelThread(e *event.Event) bool {
	if e.Exec == nil || len(e.Exec.Argv) > 0 {
		return false
	}
	exe := filepath.Join(s.procRoot, strconv.FormatUint(uint64(e.PID), 10), "exe")
	if _, err := os.Stat(exe); err == nil {
		return false
	} else if !errors.Is(err, fs.ErrNotExist) {
		return false
	}
	return true
}

func (s *Scope) remember(pid uint32) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if len(s.kthreads) >= maxKernelThreads {
		// Full. The cost is an unsuppressed exit event for one kernel thread,
		// which is a far better failure than a map that grows with pid churn.
		return
	}
	s.kthreads[pid] = struct{}{}
}

func (s *Scope) forget(pid uint32) {
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.kthreads, pid)
}

func (s *Scope) isKernelThread(pid uint32) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	_, known := s.kthreads[pid]
	return known
}
