package diff

import "fmt"

// Unified returns the differences between a and b as unified diff hunks, each
// with the given number of context lines around the changes. Changes whose
// context would touch or overlap share a hunk. Line numbers start at 1 and
// counts are always printed. The result is empty when a and b are equal.
func Unified(a, b []string, context int) string {
	ops := Compute(a, b)
	aPos := make([]int, len(ops))
	bPos := make([]int, len(ops))
	ai, bi := 0, 0
	for i, op := range ops {
		aPos[i], bPos[i] = ai, bi
		if op.Kind != '+' {
			ai++
		}
		if op.Kind != '-' {
			bi++
		}
	}
	var changes []int
	for i, op := range ops {
		if op.Kind != ' ' {
			changes = append(changes, i)
		}
	}
	if len(changes) == 0 {
		return ""
	}
	out := ""
	first, last := changes[0], changes[0]
	flush := func() {
		lo := max(first-context, 0)
		hi := min(last+context+1, len(ops))
		aCount, bCount := 0, 0
		for _, op := range ops[lo:hi] {
			if op.Kind != '+' {
				aCount++
			}
			if op.Kind != '-' {
				bCount++
			}
		}
		out += fmt.Sprintf("@@ -%d,%d +%d,%d @@\n", start(aPos[lo], aCount), aCount, start(bPos[lo], bCount), bCount)
		for _, op := range ops[lo:hi] {
			out += string(op.Kind) + op.Line + "\n"
		}
	}
	for _, c := range changes[1:] {
		if c-last-1 <= 2*context {
			last = c
			continue
		}
		flush()
		first, last = c, c
	}
	flush()
	return out
}

// start converts the number of lines before a range into the number printed in
// a header: the first line of the range, or the line before it when the range
// is empty.
func start(before, count int) int {
	if count == 0 {
		return before
	}
	return before + 1
}
