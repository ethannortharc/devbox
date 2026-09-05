package capture

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strconv"
	"testing"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

// list is a Source that emits a fixed slice and finishes. Enough to drive the
// filter without a kernel, a file, or a clock.
type list struct {
	events  []*event.Event
	domains []event.Type
}

func (l *list) Name() string { return "list" }

func (l *list) Domains() []event.Type {
	if l.domains == nil {
		return []event.Type{event.TypeFile, event.TypeExec}
	}
	return l.domains
}

func (l *list) Run(ctx context.Context, out chan<- *event.Event) error {
	for _, e := range l.events {
		if err := Send(ctx, out, e); err != nil {
			return err
		}
	}
	return nil
}

func fileEvent(pid uint32, path string) *event.Event {
	return &event.Event{
		TSWall: "2026-09-05T00:00:00.000Z", BoxID: "b", PID: pid, TID: pid,
		Type: event.TypeFile,
		File: &event.File{Path: path, Op: "open"},
	}
}

func execEvent(pid uint32, argv ...string) *event.Event {
	return &event.Event{
		TSWall: "2026-09-05T00:00:00.000Z", BoxID: "b", PID: pid, TID: pid,
		Type: event.TypeExec,
		Exec: &event.Exec{Path: "/bin/sh", Argv: argv},
	}
}

func exitEvent(pid uint32) *event.Event {
	return &event.Event{
		TSWall: "2026-09-05T00:00:01.000Z", BoxID: "b", PID: pid, TID: pid,
		Type: event.TypeExit,
	}
}

// paths runs a scope over file events and returns what survived.
func paths(t *testing.T, scope *Scope, count int) []string {
	t.Helper()
	got := collect(t, scope, count, 2*time.Second)
	kept := make([]string, 0, len(got))
	for _, e := range got {
		kept = append(kept, e.File.Path)
	}
	return kept
}

func TestFileScopeKeepsOnlyPathsUnderItsPrefixes(t *testing.T) {
	t.Parallel()

	source := &list{events: []*event.Event{
		fileEvent(10, "/workspace/main.go"),
		fileEvent(10, "/workspace"),
		// The prefix names a subtree, not a string the path starts with.
		fileEvent(10, "/workspaces/other/main.go"),
		fileEvent(10, "/workspace-backup/main.go"),
		fileEvent(10, "/nix/store/abc/lib.so"),
		fileEvent(10, "/dev/null"),
		fileEvent(10, "/home/dev/.bashrc"),
		// Cleaned before it is judged, so a traversal cannot smuggle a system
		// path in under an allowed prefix.
		fileEvent(10, "/workspace/../etc/shadow"),
		fileEvent(10, "/workspace/./src/lib.rs"),
	}}
	scope := NewScope(source, []string{"/workspace", "/home/dev"})

	kept := paths(t, scope, 4)
	want := []string{
		"/workspace/main.go", "/workspace", "/home/dev/.bashrc",
		"/workspace/./src/lib.rs",
	}
	if len(kept) != len(want) {
		t.Fatalf("kept %v, want %v", kept, want)
	}
	for i, path := range want {
		if kept[i] != path {
			t.Fatalf("kept %v, want %v", kept, want)
		}
	}
	if got := scope.FileOutOfScope(); got != 5 {
		t.Fatalf("file_out_of_scope = %d, want 5", got)
	}
	if got := scope.KernelNoise(); got != 0 {
		t.Fatalf("kernel noise = %d, want 0", got)
	}
}

func TestAScopeWithATrailingSlashMatchesTheSameSubtree(t *testing.T) {
	t.Parallel()

	prefixes, err := ParseFileScope("/workspace/, /home/dev , /workspace")
	if err != nil {
		t.Fatalf("ParseFileScope: %v", err)
	}
	// Cleaned and de-duplicated, so a trailing slash or a repeat is not a
	// second rule that has to agree with the first.
	if len(prefixes) != 2 || prefixes[0] != "/workspace" || prefixes[1] != "/home/dev" {
		t.Fatalf("prefixes = %v", prefixes)
	}
}

func TestARelativeScopeEntryIsRefusedRatherThanIgnored(t *testing.T) {
	t.Parallel()

	// Dropping it would leave the list empty, which admits everything — so a
	// typo would silently restore the flood the scope exists to stop.
	if _, err := ParseFileScope("/workspace,workspace"); err == nil {
		t.Fatal("a relative scope entry was accepted")
	}
}

func TestAnEmptyScopeAdmitsEveryPath(t *testing.T) {
	t.Parallel()

	prefixes, err := ParseFileScope("  ,  ")
	if err != nil {
		t.Fatalf("ParseFileScope: %v", err)
	}
	if len(prefixes) != 0 {
		t.Fatalf("prefixes = %v, want none", prefixes)
	}
	scope := NewScope(&list{events: []*event.Event{
		fileEvent(10, "/dev/null"),
		fileEvent(10, "/workspace/main.go"),
	}}, prefixes)

	if kept := paths(t, scope, 2); len(kept) != 2 {
		t.Fatalf("kept %v, want both paths", kept)
	}
	if got := scope.FileOutOfScope(); got != 0 {
		t.Fatalf("an empty scope suppressed %d event(s)", got)
	}
}

func TestARelativePathIsOutOfScope(t *testing.T) {
	t.Parallel()

	// The openat tracepoint records the pathname and not the directory fd it
	// resolves against, so `lib` cannot be placed in a subtree here.
	scope := NewScope(&list{events: []*event.Event{
		fileEvent(10, "lib"),
		fileEvent(10, ".."),
		fileEvent(10, ""),
		fileEvent(10, "/workspace/main.go"),
	}}, []string{"/workspace"})

	kept := paths(t, scope, 1)
	if len(kept) != 1 || kept[0] != "/workspace/main.go" {
		t.Fatalf("kept %v", kept)
	}
	if got := scope.FileOutOfScope(); got != 3 {
		t.Fatalf("file_out_of_scope = %d, want 3", got)
	}
}

func TestScopeLeavesEveryOtherEventTypeAlone(t *testing.T) {
	t.Parallel()

	dns := &event.Event{
		TSWall: "2026-09-05T00:00:00.000Z", BoxID: "b", PID: 9, Type: event.TypeDNS,
		Net: &event.Net{Proto: "udp", QName: "pypi.org"},
	}
	scope := NewScope(&list{events: []*event.Event{dns}}, []string{"/workspace"})
	if got := collect(t, scope, 1, 2*time.Second); len(got) != 1 || got[0].Type != event.TypeDNS {
		t.Fatalf("a DNS event did not survive the file scope: %v", got)
	}
}

// procLike builds a /proc-shaped tree: `live` pids have an exe symlink that
// resolves, `kthreads` have one that dangles.
//
// The dangling half is the point. A kernel thread has a `/proc/<pid>/exe`
// symlink exactly like a real process — it simply points at nothing — and a
// tree where kernel threads had no link at all let a check that never followed
// the link pass this test while suppressing nothing on a live box.
func procLike(t *testing.T, live []uint32, kthreads []uint32) string {
	t.Helper()
	root := t.TempDir()
	target := filepath.Join(root, "real-binary")
	if err := os.WriteFile(target, []byte("ELF"), 0o755); err != nil {
		t.Fatal(err)
	}
	link := func(pid uint32, to string) {
		dir := filepath.Join(root, strconv.FormatUint(uint64(pid), 10))
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.Symlink(to, filepath.Join(dir, "exe")); err != nil {
			t.Fatal(err)
		}
	}
	for _, pid := range live {
		link(pid, target)
	}
	for _, pid := range kthreads {
		link(pid, filepath.Join(root, "nothing-is-here"))
	}
	return root
}

func TestKernelThreadExecsAndTheirExitsAreSuppressed(t *testing.T) {
	t.Parallel()

	// pid 42 is a real process; pid 7 has no argv and no exe, which is what a
	// kernel thread looks like from /proc.
	source := &list{
		events: []*event.Event{
			execEvent(7),
			exitEvent(7),
			execEvent(42, "/bin/sh", "-c", "true"),
			exitEvent(42),
		},
		domains: []event.Type{event.TypeExec, event.TypeExit},
	}
	scope := NewScope(source, []string{"/workspace"})
	scope.procRoot = procLike(t, []uint32{42}, []uint32{7})

	got := collect(t, scope, 2, 2*time.Second)
	if len(got) != 2 {
		t.Fatalf("kept %d event(s), want the real process's exec and exit", len(got))
	}
	for _, e := range got {
		if e.PID != 42 {
			t.Fatalf("a kernel thread event survived: %+v", e)
		}
	}
	if want := uint64(2); scope.KernelNoise() != want {
		t.Fatalf("kernel noise = %d, want %d", scope.KernelNoise(), want)
	}
	if got := scope.FileOutOfScope(); got != 0 {
		t.Fatalf("the kernel-thread rule counted %d file event(s)", got)
	}
}

func TestAnExecWithNoArgvButALiveExeIsKept(t *testing.T) {
	t.Parallel()

	// Missing argv alone is not proof: a process can clear it. The /proc
	// lookup is what separates that from a kernel thread.
	source := &list{
		events:  []*event.Event{execEvent(11)},
		domains: []event.Type{event.TypeExec},
	}
	scope := NewScope(source, nil)
	scope.procRoot = procLike(t, []uint32{11}, nil)

	if got := collect(t, scope, 1, 2*time.Second); len(got) != 1 {
		t.Fatalf("a live process with no argv was suppressed")
	}
	if got := scope.KernelNoise(); got != 0 {
		t.Fatalf("kernel noise = %d, want 0", got)
	}
}

func TestAReusedPidStopsBeingTreatedAsAKernelThread(t *testing.T) {
	t.Parallel()

	scope := NewScope(&list{}, nil)
	scope.procRoot = procLike(t, nil, []uint32{7})
	if scope.admit(execEvent(7)) {
		t.Fatal("the kernel thread's exec was forwarded")
	}
	if !scope.isKernelThread(7) {
		t.Fatal("the suppressed pid was not remembered")
	}

	// The same pid, now a real process with an argv: its exit must survive.
	if !scope.admit(execEvent(7, "/bin/sh")) {
		t.Fatal("a real exec was suppressed")
	}
	if scope.isKernelThread(7) {
		t.Fatal("the pid is still remembered as a kernel thread")
	}
	if !scope.admit(exitEvent(7)) {
		t.Fatal("the reused pid's exit was suppressed")
	}
}

func TestScopeDelegatesIdentityAndCloseToItsSource(t *testing.T) {
	t.Parallel()

	inner := &closableList{list: list{domains: []event.Type{event.TypeFile}}}
	scope := NewScope(inner, []string{"/workspace"})
	if scope.Name() != "list" {
		t.Fatalf("Name() = %q, want the wrapped source's name", scope.Name())
	}
	if domains := scope.Domains(); len(domains) != 1 || domains[0] != event.TypeFile {
		t.Fatalf("Domains() = %v", domains)
	}
	if err := scope.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}
	if !inner.closed {
		t.Fatal("closing the scope did not release the source it wraps")
	}
}

type closableList struct {
	list
	closed bool
}

func (c *closableList) Close() error {
	c.closed = true
	return nil
}

func TestScopeReturnsItsSourcesFailure(t *testing.T) {
	t.Parallel()

	// A filter must not swallow the failure of what it filters: capture has
	// stopped, and the agent has to say so rather than look idle.
	boom := errors.New("ring buffer closed")
	scope := NewScope(&failing{err: boom}, []string{"/workspace"})
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	if err := scope.Run(ctx, make(chan *event.Event, 1)); !errors.Is(err, boom) {
		t.Fatalf("Run = %v, want the source's own error", err)
	}
}
