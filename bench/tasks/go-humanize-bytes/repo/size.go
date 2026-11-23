// Package humanize formats quantities for people.
package humanize

import "fmt"

var units = []string{"B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"}

// FormatBytes returns a short text for a byte count, for example "1.5 KiB".
func FormatBytes(n int64) string {
	v := n
	i := 0
	for v >= 1000 && i < len(units)-1 {
		v /= 1024
		i++
	}
	return fmt.Sprintf("%d %s", v, units[i])
}
