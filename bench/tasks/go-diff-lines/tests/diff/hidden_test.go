package diff

import "testing"

func check(t *testing.T, a, b []string, context int, want string) {
	t.Helper()
	if got := Unified(a, b, context); got != want {
		t.Fatalf("got:\n%s\nwant:\n%s", got, want)
	}
}

func TestHiddenInsertIntoEmptyFileWithAnyContext(t *testing.T) {
	for _, ctx := range []int{0, 1, 3} {
		check(t, nil, lines("x"), ctx, "@@ -0,0 +1,1 @@\n+x\n")
	}
}

func TestHiddenDeleteEverything(t *testing.T) {
	check(t, lines("x", "y"), nil, 2, "@@ -1,2 +0,0 @@\n-x\n-y\n")
}

func TestHiddenDeleteLastLine(t *testing.T) {
	check(t, lines("a", "b", "c"), lines("a", "b"), 1, "@@ -2,2 +2,1 @@\n b\n-c\n")
	check(t, lines("a", "b", "c"), lines("a", "b"), 0, "@@ -3,1 +2,0 @@\n-c\n")
}

func TestHiddenInsertAtStart(t *testing.T) {
	check(t, lines("a", "b"), lines("X", "a", "b"), 1, "@@ -1,1 +1,2 @@\n+X\n a\n")
	check(t, lines("a", "b"), lines("X", "a", "b"), 0, "@@ -0,0 +1,1 @@\n+X\n")
}

func TestHiddenAppendAtEnd(t *testing.T) {
	check(t, lines("a", "b"), lines("a", "b", "Z"), 1, "@@ -2,1 +2,2 @@\n b\n+Z\n")
	check(t, lines("a", "b"), lines("a", "b", "Z"), 0, "@@ -2,0 +3,1 @@\n+Z\n")
}

func TestHiddenDeleteFirstLine(t *testing.T) {
	check(t, lines("a", "b", "c"), lines("b", "c"), 1, "@@ -1,2 +1,1 @@\n-a\n b\n")
}

func TestHiddenInsertBetweenLinesWithoutContext(t *testing.T) {
	check(t, lines("a", "b"), lines("a", "X", "b"), 0, "@@ -1,0 +2,1 @@\n+X\n")
}

func TestHiddenCloseChangesShareOneHunk(t *testing.T) {
	a := lines("1", "2", "3", "4", "5", "6")
	b := lines("1", "X", "3", "4", "Y", "6")
	// gap of two unchanged lines, context 1: the contexts touch
	check(t, a, b, 1, "@@ -1,6 +1,6 @@\n 1\n-2\n+X\n 3\n 4\n-5\n+Y\n 6\n")
}

func TestHiddenChangesThreeApartStaySeparate(t *testing.T) {
	a := lines("1", "2", "3", "4", "5", "6", "7")
	b := lines("1", "X", "3", "4", "5", "Y", "7")
	check(t, a, b, 1, "@@ -1,3 +1,3 @@\n 1\n-2\n+X\n 3\n@@ -5,3 +5,3 @@\n 5\n-6\n+Y\n 7\n")
}
