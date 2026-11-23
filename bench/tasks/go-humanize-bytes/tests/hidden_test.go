package humanize

import (
	"math"
	"testing"
)

func TestHiddenBoundaries(t *testing.T) {
	cases := []struct {
		in   int64
		want string
	}{
		{999, "999 B"},
		{1000, "1000 B"},
		{1023, "1023 B"},
		{1024, "1.0 KiB"},
		{1025, "1.0 KiB"},
		{1536, "1.5 KiB"},
		{1 << 20, "1.0 MiB"},
		{1048524, "1023.9 KiB"},
		{1048575, "1.0 MiB"},
		{1 << 50, "1.0 PiB"},
		{1 << 60, "1.0 EiB"},
		{math.MaxInt64, "8.0 EiB"},
		{-1536, "-1.5 KiB"},
		{-5, "-5 B"},
	}
	for _, c := range cases {
		if got := FormatBytes(c.in); got != c.want {
			t.Errorf("FormatBytes(%d) = %q, want %q", c.in, got, c.want)
		}
	}
}
