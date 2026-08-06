package buildinfo

import (
	"strings"
	"testing"
)

func TestStringIncludesServiceVersionAndCommit(t *testing.T) {
	t.Parallel()

	got := String(Obsd)
	for _, want := range []string{"devbox-obsd", Version, Commit} {
		if !strings.Contains(got, want) {
			t.Errorf("String(Obsd) = %q, missing %q", got, want)
		}
	}
}

func TestCompatible(t *testing.T) {
	t.Parallel()

	cases := []struct {
		name        string
		agent, host string
		want        bool
	}{
		{"identical releases", "0.1.3", "0.1.3", true},
		{"different releases", "0.1.3", "0.2.0", false},
		{"dev agent against release host", devVersion, "0.2.0", true},
		{"release agent against dev host", "0.2.0", devVersion, true},
		{"both dev", devVersion, devVersion, true},
		// A prefix match would wrongly pass here; equality must be exact.
		{"prefix is not a match", "0.1.3", "0.1.30", false},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			if got := Compatible(tc.agent, tc.host); got != tc.want {
				t.Errorf("Compatible(%q, %q) = %v, want %v",
					tc.agent, tc.host, got, tc.want)
			}
		})
	}
}
