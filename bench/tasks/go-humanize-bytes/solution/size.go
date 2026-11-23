// Package humanize formats quantities for people.
package humanize

import (
	"fmt"
	"math"
)

var units = []string{"B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"}

// FormatBytes returns a short text for a byte count, for example "1.5 KiB".
// Units are powers of 1024 and sizes above one KiB have one decimal.
func FormatBytes(n int64) string {
	sign := ""
	v := float64(n)
	if n < 0 {
		sign = "-"
		v = -v
	}
	if v < 1024 {
		return fmt.Sprintf("%d B", n)
	}
	i := 0
	for v >= 1024 && i < len(units)-1 {
		v /= 1024
		i++
	}
	v = math.Round(v*10) / 10
	if v >= 1024 && i < len(units)-1 {
		v = math.Round(v/1024*10) / 10
		i++
	}
	return fmt.Sprintf("%s%.1f %s", sign, v, units[i])
}
