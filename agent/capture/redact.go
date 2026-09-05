package capture

import "strings"

// Redaction of secrets that reach an argv.
//
// Every runtime's exec takes an argv and no environment, so devbox puts the
// broker's variables on the *command line* (`env -- K=V … cmd`). That is the
// only form that works uniformly, and it means a box's own credentials appear
// in the `exec` events the agent captures — in the run report's process tree,
// in the exported JSON, and in `devbox watch --tree`.
//
// Nothing here is a containment boundary. The box can read `/proc/*/cmdline`
// and see the same bytes, and the broker's token is per box and rotates on
// restart. What this protects is the *receipt*: v5's claim is that a run report
// can be handed to someone else, and a report carrying a live token cannot be.
//
// The same rule exists in Rust (`obs::redact`), because the events already in
// a store were written before this code did. Both are tested against the same
// shapes; a change to one that is not made to the other shows up as a test
// failure rather than as a token in a file someone forwarded.

// redacted is what replaces a secret's value. Short, obviously not a value,
// and the same in both implementations so a reader who greps for it in one
// output finds it in the others.
const redacted = "***"

// sensitiveEnvSuffixes name a variable whose value is a credential.
//
// Suffix rather than an allowlist: the set of things that end in `_TOKEN` is
// open, and a variable devbox has never heard of is exactly the one nobody
// will remember to add. A false positive costs a reader the value of
// `SORT_KEY`; a false negative costs them a live credential in a file they
// forwarded.
var sensitiveEnvSuffixes = []string{"_TOKEN", "_SECRET", "_KEY", "_CREDENTIALS"}

// sensitiveEnvContains is checked anywhere in the name, not just at the end:
// `PGPASSWORD` and `MYSQL_PWD_PASSWORD_FILE` are both real spellings.
var sensitiveEnvContains = []string{"PASSWORD"}

// sensitiveEnvExact are the bare names the suffix rule cannot reach.
//
// `*_TOKEN` does not match `TOKEN`, and `TOKEN=abc` is not a plausible sort
// key — it is a credential with nothing in front of it. `KEY` is deliberately
// absent: on its own it is far more often a map key or a sort field.
var sensitiveEnvExact = []string{"TOKEN", "SECRET", "PASSWD", "CREDENTIALS"}

// sensitiveHeaderSuffixes name a header whose value authenticates.
var sensitiveHeaderSuffixes = []string{"-token", "-key", "-secret", "-password"}

// sensitiveHeaders are the exact names that carry no such suffix.
var sensitiveHeaders = []string{
	"authorization",
	"proxy-authorization",
	"cookie",
	"set-cookie",
}

// SensitiveEnvName reports whether a variable's value should be redacted.
func SensitiveEnvName(name string) bool {
	if name == "" {
		return false
	}
	upper := strings.ToUpper(name)
	for _, exact := range sensitiveEnvExact {
		if upper == exact {
			return true
		}
	}
	for _, needle := range sensitiveEnvContains {
		if strings.Contains(upper, needle) {
			return true
		}
	}
	for _, suffix := range sensitiveEnvSuffixes {
		if strings.HasSuffix(upper, suffix) {
			return true
		}
	}
	return false
}

// SensitiveHeaderName reports whether a header's value should be redacted.
func SensitiveHeaderName(name string) bool {
	lower := strings.ToLower(strings.TrimSpace(name))
	if lower == "" {
		return false
	}
	for _, exact := range sensitiveHeaders {
		if lower == exact {
			return true
		}
	}
	for _, suffix := range sensitiveHeaderSuffixes {
		if strings.HasSuffix(lower, suffix) {
			return true
		}
	}
	return false
}

// RedactArgv redacts every argv word in place, reporting whether it changed
// anything.
//
// Returns the same slice: the caller owns an event it is about to send, and
// allocating a second one per exec on a busy box is not free.
func RedactArgv(argv []string) bool {
	changed := false
	for i, word := range argv {
		clean := RedactWord(word)
		if clean != word {
			argv[i] = clean
			changed = true
		}
	}
	return changed
}

// RedactWord removes credentials from one argv word.
//
// Scans *inside* the word rather than treating it as a single `NAME=value`.
// The outermost shell line a runtime generates carries the whole command as
// one quoted argument —
//
//	bash -c "cd /home; exec /bin/bash -l -c 'env -- DEVBOX_BROKER_TOKEN=… cmd'"
//
// — so a word-level split sees one enormous word with no `=` at its head, and
// that is precisely the shape that put a live token into a run report.
func RedactWord(word string) string {
	if word == "" {
		return word
	}
	out := redactAssignments(word)
	return redactHeaders(out)
}

// redactAssignments finds `NAME=value` anywhere in a word.
func redactAssignments(word string) string {
	var b strings.Builder
	i := 0
	for i < len(word) {
		equals := strings.IndexByte(word[i:], '=')
		if equals < 0 {
			b.WriteString(word[i:])
			break
		}
		equals += i

		// Walk left over the name. Stopping at the first character that cannot
		// be in one is what keeps `--env=FOO` and `x DEVBOX_TOKEN=y` both
		// resolving to the name and not to the flag or the space before it.
		start := equals
		for start > i && isEnvNameByte(word[start-1]) {
			start--
		}
		name := word[start:equals]

		if !SensitiveEnvName(name) {
			b.WriteString(word[i : equals+1])
			i = equals + 1
			continue
		}

		b.WriteString(word[i:equals])
		b.WriteByte('=')
		b.WriteString(redacted)
		i = equals + 1 + valueLen(word[equals+1:])
	}
	return b.String()
}

// redactHeaders finds `Name: value` anywhere in a word.
func redactHeaders(word string) string {
	var b strings.Builder
	i := 0
	for i < len(word) {
		colon := strings.IndexByte(word[i:], ':')
		if colon < 0 {
			b.WriteString(word[i:])
			break
		}
		colon += i

		start := colon
		for start > i && isHeaderNameByte(word[start-1]) {
			start--
		}
		name := word[start:colon]

		if !SensitiveHeaderName(name) {
			b.WriteString(word[i : colon+1])
			i = colon + 1
			continue
		}

		b.WriteString(word[i:colon])
		b.WriteString(": ")
		b.WriteString(redacted)
		// A header value may contain spaces (`Bearer abc`), so it runs to the
		// end of the word or to the quote that closes it — unlike an
		// assignment, which a space terminates.
		i = colon + 1 + headerValueLen(word[colon+1:])
	}
	return b.String()
}

// valueLen is how much of `rest` belongs to an assignment's value.
//
// Whitespace ends it, and so does a quote: inside the one-word form the value
// is followed by the next assignment or by the closing quote of the shell
// string it lives in, and swallowing that quote would corrupt the rendering of
// everything after it.
func valueLen(rest string) int {
	for i := 0; i < len(rest); i++ {
		switch rest[i] {
		case ' ', '\t', '\n', '\r', '\'', '"':
			return i
		}
	}
	return len(rest)
}

// headerValueLen is how much of `rest` belongs to a header's value.
func headerValueLen(rest string) int {
	for i := 0; i < len(rest); i++ {
		switch rest[i] {
		case '\n', '\r', '\'', '"':
			return i
		}
	}
	return len(rest)
}

func isEnvNameByte(c byte) bool {
	return c == '_' ||
		(c >= 'A' && c <= 'Z') ||
		(c >= 'a' && c <= 'z') ||
		(c >= '0' && c <= '9')
}

func isHeaderNameByte(c byte) bool {
	return c == '-' || c == '_' ||
		(c >= 'A' && c <= 'Z') ||
		(c >= 'a' && c <= 'z') ||
		(c >= '0' && c <= '9')
}
