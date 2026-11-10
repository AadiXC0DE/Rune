// Package diff compares two lists of lines and prints the changes as hunks.
package diff

// Op is one step of an edit script: a line kept (' '), removed ('-') or added ('+').
type Op struct {
	Kind byte
	Line string
}

// Compute returns an edit script turning a into b, built from a longest
// common subsequence. Removals come before additions inside a changed stretch.
func Compute(a, b []string) []Op {
	n, m := len(a), len(b)
	best := make([][]int, n+1)
	for i := range best {
		best[i] = make([]int, m+1)
	}
	for i := n - 1; i >= 0; i-- {
		for j := m - 1; j >= 0; j-- {
			if a[i] == b[j] {
				best[i][j] = best[i+1][j+1] + 1
			} else if best[i+1][j] >= best[i][j+1] {
				best[i][j] = best[i+1][j]
			} else {
				best[i][j] = best[i][j+1]
			}
		}
	}
	var ops []Op
	i, j := 0, 0
	for i < n || j < m {
		switch {
		case i < n && j < m && a[i] == b[j]:
			ops = append(ops, Op{' ', a[i]})
			i++
			j++
		case i < n && (j == m || best[i+1][j] >= best[i][j+1]):
			ops = append(ops, Op{'-', a[i]})
			i++
		default:
			ops = append(ops, Op{'+', b[j]})
			j++
		}
	}
	return ops
}
