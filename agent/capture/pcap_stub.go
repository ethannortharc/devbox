//go:build !linux

package capture

import (
	"context"
	"fmt"
	"io"
)

// CapturePCAP validates the request, then reports that AF_PACKET is unavailable.
//
//nolint:revive // PCAP is the operation and the package also owns other capture sources.
func CapturePCAP(_ context.Context, filter PCAPFilter, _ io.Writer) (int, error) {
	if err := filter.validate(); err != nil {
		return 0, err
	}
	return 0, fmt.Errorf("flow pcap capture requires Linux AF_PACKET")
}
