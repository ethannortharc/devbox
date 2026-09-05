// Package buildinfo carries the identity the Go services report about
// themselves.
//
// The observability agent and the host binary must agree on a build: §7.3 of
// the v4 design pins the agent version to the host binary that pushed it, and
// the collector refuses a stream from a mismatched agent rather than decoding
// events with the wrong layout. Version and Commit are stamped at link time:
//
//	go build -ldflags "-X github.com/ethannortharc/devbox/internal/buildinfo.Version=0.1.3"
package buildinfo

import "fmt"

// Values overridden at link time. The defaults are what a plain `go build`
// or `go test` produces, and they are deliberately obvious.
var (
	// Version is the devbox release this binary belongs to.
	Version = "0.0.0-dev"
	// Commit is the short git revision it was built from.
	Commit = "unknown"
)

// Service names a Go service that ships inside devbox.
type Service string

const (
	// Obsd is the in-guest eBPF observability agent.
	Obsd Service = "devbox-obsd"
)

// String renders a one-line identity banner, e.g.
// "devbox-obsd 0.1.3 (a1b2c3d)".
func String(svc Service) string {
	return fmt.Sprintf("%s %s (%s)", svc, Version, Commit)
}

// Compatible reports whether an agent built at agentVersion may talk to a host
// built at hostVersion.
//
// The wire format is versioned with the release, so the rule is exact equality
// — a "close enough" match is how you get a decoder silently misreading a
// struct. Development builds are exempt so a locally built agent can be tested
// against a released host.
func Compatible(agentVersion, hostVersion string) bool {
	if agentVersion == devVersion || hostVersion == devVersion {
		return true
	}
	return agentVersion == hostVersion
}

const devVersion = "0.0.0-dev"
