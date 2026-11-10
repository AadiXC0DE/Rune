package config

import (
	"bufio"
	"fmt"
	"io"
	"strings"
)

// ParseFile reads lines of the form "key = value". Blank lines and lines
// starting with # are skipped. Keys may be dotted, as in "db.host".
func ParseFile(r io.Reader) (map[string]string, error) {
	out := map[string]string{}
	sc := bufio.NewScanner(r)
	n := 0
	for sc.Scan() {
		n++
		line := strings.TrimSpace(sc.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		key, value, ok := strings.Cut(line, "=")
		if !ok {
			return nil, fmt.Errorf("line %d: expected key = value", n)
		}
		out[strings.TrimSpace(key)] = strings.TrimSpace(value)
	}
	return out, sc.Err()
}
