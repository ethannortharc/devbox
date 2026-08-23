//go:build !linux

package capture

import "time"

// NewPacket reports that the AF_PACKET source is unavailable off Linux.
func NewPacket(_ string, _ time.Time) (Source, error) {
	return nil, &ErrUnsupported{
		Source: "packet",
		Reason: "AF_PACKET capture is available only inside a Linux box",
	}
}
