package ring

import (
	"reflect"
	"testing"
)

func TestFillsToCapacity(t *testing.T) {
	b := New[int](3)
	for i := 1; i <= 3; i++ {
		if _, ok := b.Push(i); ok {
			t.Fatalf("push %d evicted from a buffer that is not full", i)
		}
	}
	if b.Len() != 3 || b.Cap() != 3 {
		t.Fatalf("Len=%d Cap=%d, want 3 and 3", b.Len(), b.Cap())
	}
	if got := b.Items(); !reflect.DeepEqual(got, []int{1, 2, 3}) {
		t.Fatalf("Items() = %v", got)
	}
}

func TestOverwritesOldest(t *testing.T) {
	b := New[string](2)
	b.Push("a")
	b.Push("b")
	old, ok := b.Push("c")
	if !ok || old != "a" {
		t.Fatalf("Push evicted %q, %v; want a, true", old, ok)
	}
	if got := b.Items(); !reflect.DeepEqual(got, []string{"b", "c"}) {
		t.Fatalf("Items() = %v", got)
	}
}
