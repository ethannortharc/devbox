package capture

import (
	"context"
	"testing"
	"time"

	"github.com/ethannortharc/devbox/agent/event"
)

func TestRedactWordCoversEveryShapeAnArgvProduces(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want string
	}{
		{
			name: "the broker token as its own argv word",
			in:   "DEVBOX_BROKER_TOKEN=sk-abc123",
			want: "DEVBOX_BROKER_TOKEN=***",
		},
		{
			// The shape that put a live token into a run report: a runtime's
			// outermost login shell carries the whole command as one quoted
			// word, so there is nothing at the head of it to split on.
			name: "buried inside a runtime's login shell line",
			in:   "cd /home || exit 1 ; exec /bin/bash -l -c 'env -- DEVBOX_BROKER_TOKEN=sk-abc123 DEVBOX_RUN_ID=01X sh -c true'",
			want: "cd /home || exit 1 ; exec /bin/bash -l -c 'env -- DEVBOX_BROKER_TOKEN=*** DEVBOX_RUN_ID=01X sh -c true'",
		},
		{
			name: "several assignments in one word",
			in:   "env -- ANTHROPIC_AUTH_TOKEN=a OPENAI_API_KEY=b DEVBOX_BROKER_URL=http://h:9 cmd",
			want: "env -- ANTHROPIC_AUTH_TOKEN=*** OPENAI_API_KEY=*** DEVBOX_BROKER_URL=http://h:9 cmd",
		},
		{
			name: "suffix rules",
			in:   "A_TOKEN=1 B_SECRET=2 C_KEY=3 D_CREDENTIALS=4 PGPASSWORD=5 lower_token=6",
			want: "A_TOKEN=*** B_SECRET=*** C_KEY=*** D_CREDENTIALS=*** PGPASSWORD=*** lower_token=***",
		},
		{
			// `*_TOKEN` does not match `TOKEN`, and a bare one is a credential
			// with nothing in front of it rather than a sort field.
			name: "bare names the suffix rule cannot reach",
			in:   "TOKEN=1 SECRET=2 CREDENTIALS=3",
			want: "TOKEN=*** SECRET=*** CREDENTIALS=***",
		},
		{
			// `KEY` alone is deliberately not sensitive: it is far more often
			// a map key or a sort field than a credential.
			name: "a bare KEY is left alone",
			in:   "sort --key=3 KEY=name",
			want: "sort --key=3 KEY=name",
		},
		{
			// The counter-example: a run's identity is not a credential, and
			// redacting it would break the one field that ties a report to its
			// events.
			name: "the run id is not a secret",
			in:   "DEVBOX_RUN_ID=01M1SCJ6KHE90VMWKN04C5QQVT",
			want: "DEVBOX_RUN_ID=01M1SCJ6KHE90VMWKN04C5QQVT",
		},
		{
			name: "other devbox variables survive",
			in:   "DEVBOX_BROKER_URL=http://host.lima.internal:7879",
			want: "DEVBOX_BROKER_URL=http://host.lima.internal:7879",
		},
		{
			name: "an authorization header",
			in:   "Authorization: Bearer sk-abc123",
			want: "Authorization: ***",
		},
		{
			name: "a header inside a quoted curl argument",
			in:   "curl -s -H 'Authorization: Bearer sk-abc123' https://api",
			want: "curl -s -H 'Authorization: ***' https://api",
		},
		{
			name: "the broker's own header",
			in:   "-H 'x-devbox-broker-token: abc123'",
			want: "-H 'x-devbox-broker-token: ***'",
		},
		{
			name: "an api key header",
			in:   "-H 'x-api-key: abc123'",
			want: "-H 'x-api-key: ***'",
		},
		{
			// A colon is not a header. A URL, a PATH and a timestamp all have
			// one, and redacting on the punctuation alone would empty them.
			name: "colons that are not headers",
			in:   "curl https://example.com:443/x PATH=/usr/bin:/bin at 10:00:00",
			want: "curl https://example.com:443/x PATH=/usr/bin:/bin at 10:00:00",
		},
		{
			name: "empty",
			in:   "",
			want: "",
		},
		{
			name: "an equals with no name",
			in:   "=value",
			want: "=value",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := RedactWord(tc.in); got != tc.want {
				t.Errorf("RedactWord(%q)\n got %q\nwant %q", tc.in, got, tc.want)
			}
			// Idempotent: the redacted form has to survive a second pass, or
			// the Rust side running over an already-clean event would corrupt
			// it.
			if got := RedactWord(RedactWord(tc.in)); got != tc.want {
				t.Errorf("RedactWord is not idempotent for %q: %q", tc.in, got)
			}
		})
	}
}

func TestRedactArgvReportsWhetherItChangedAnything(t *testing.T) {
	argv := []string{"sh", "-c", "DEVBOX_BROKER_TOKEN=abc true"}
	if !RedactArgv(argv) {
		t.Fatal("a token in the argv must be reported as redacted")
	}
	if argv[2] != "DEVBOX_BROKER_TOKEN=*** true" {
		t.Fatalf("argv not redacted in place: %q", argv[2])
	}

	clean := []string{"sh", "-c", "echo hi"}
	if RedactArgv(clean) {
		t.Fatal("a clean argv must not be reported as redacted")
	}
}

// The scope is the one place every source's events pass through, so this is
// the assertion that a new source cannot be added around the redaction.
func TestScopeRedactsExecArgvFromEverySource(t *testing.T) {
	source := &staticSource{events: []*event.Event{
		{
			Type: event.TypeExec,
			PID:  900,
			Exec: &event.Exec{
				Path: "/bin/bash",
				Argv: []string{"bash", "-c", "env -- DEVBOX_BROKER_TOKEN=sk-abc123 sh -c true"},
			},
		},
		{
			Type: event.TypeExec,
			PID:  901,
			Exec: &event.Exec{Path: "/bin/true", Argv: []string{"true"}},
		},
	}}

	scope := NewScope(source, nil)
	scope.procRoot = t.TempDir() // no /proc, so nothing reads as a kernel thread

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	out := make(chan *event.Event, 4)
	if err := scope.Run(ctx, out); err != nil {
		t.Fatalf("scope run: %v", err)
	}
	close(out)

	var got []*event.Event
	for e := range out {
		got = append(got, e)
	}
	if len(got) != 2 {
		t.Fatalf("expected 2 events, got %d", len(got))
	}
	if got[0].Exec.Argv[2] != "env -- DEVBOX_BROKER_TOKEN=*** sh -c true" {
		t.Errorf("argv not redacted: %q", got[0].Exec.Argv[2])
	}
	if scope.ArgvRedacted() != 1 {
		t.Errorf("ArgvRedacted = %d, want 1", scope.ArgvRedacted())
	}
}

type staticSource struct{ events []*event.Event }

func (s *staticSource) Name() string          { return "static" }
func (s *staticSource) Domains() []event.Type { return []event.Type{event.TypeExec} }
func (s *staticSource) Run(ctx context.Context, out chan<- *event.Event) error {
	for _, e := range s.events {
		if err := Send(ctx, out, e); err != nil {
			return err
		}
	}
	return nil
}
