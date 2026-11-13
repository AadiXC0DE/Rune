package diff

import "testing"

func lines(s ...string) []string { return s }

func TestChangeInTheMiddle(t *testing.T) {
	a := lines("a", "b", "c", "d", "e")
	b := lines("a", "b", "X", "d", "e")
	want := "@@ -2,3 +2,3 @@\n b\n-c\n+X\n d\n"
	if got := Unified(a, b, 1); got != want {
		t.Fatalf("got:\n%s\nwant:\n%s", got, want)
	}
}

func TestEqualInputsGiveNothing(t *testing.T) {
	a := lines("a", "b")
	if got := Unified(a, a, 3); got != "" {
		t.Fatalf("got %q", got)
	}
}

func TestEmptyFileGainsLines(t *testing.T) {
	want := "@@ -0,0 +1,2 @@\n+x\n+y\n"
	if got := Unified(nil, lines("x", "y"), 3); got != want {
		t.Fatalf("got:\n%s\nwant:\n%s", got, want)
	}
}

func TestDistantChangesMakeTwoHunks(t *testing.T) {
	a := lines("1", "2", "3", "4", "5", "6", "7", "8", "9")
	b := lines("1", "X", "3", "4", "5", "6", "7", "Y", "9")
	want := "@@ -1,3 +1,3 @@\n 1\n-2\n+X\n 3\n@@ -7,3 +7,3 @@\n 7\n-8\n+Y\n 9\n"
	if got := Unified(a, b, 1); got != want {
		t.Fatalf("got:\n%s\nwant:\n%s", got, want)
	}
}
