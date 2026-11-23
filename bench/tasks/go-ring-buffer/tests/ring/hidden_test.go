package ring

import (
	"reflect"
	"testing"
)

func TestHiddenCapacityOne(t *testing.T) {
	b := New[int](1)
	if _, ok := b.Push(1); ok {
		t.Fatal("first push into capacity one evicted")
	}
	old, ok := b.Push(2)
	if !ok || old != 1 {
		t.Fatalf("second push evicted %d, %v; want 1, true", old, ok)
	}
	if got := b.Items(); !reflect.DeepEqual(got, []int{2}) {
		t.Fatalf("Items() = %v", got)
	}
	if b.Len() != 1 {
		t.Fatalf("Len() = %d", b.Len())
	}
}

func TestHiddenWrapTwice(t *testing.T) {
	b := New[int](3)
	var evicted []int
	for i := 1; i <= 8; i++ {
		if old, ok := b.Push(i); ok {
			evicted = append(evicted, old)
		}
	}
	if !reflect.DeepEqual(evicted, []int{1, 2, 3, 4, 5}) {
		t.Fatalf("evicted %v", evicted)
	}
	if got := b.Items(); !reflect.DeepEqual(got, []int{6, 7, 8}) {
		t.Fatalf("Items() = %v", got)
	}
}

func TestHiddenEmpty(t *testing.T) {
	b := New[int](4)
	if got := b.Items(); len(got) != 0 {
		t.Fatalf("Items() = %v", got)
	}
	called := false
	b.Each(func(int) bool { called = true; return true })
	if called {
		t.Fatal("Each visited an item of an empty buffer")
	}
	if b.Len() != 0 {
		t.Fatalf("Len() = %d", b.Len())
	}
}

func TestHiddenEachOrderAndStop(t *testing.T) {
	b := New[int](4)
	for i := 1; i <= 6; i++ {
		b.Push(i)
	}
	var seen []int
	b.Each(func(v int) bool {
		seen = append(seen, v)
		return len(seen) < 3
	})
	if !reflect.DeepEqual(seen, []int{3, 4, 5}) {
		t.Fatalf("Each saw %v", seen)
	}
}

func TestHiddenPartialFillAfterWrapKeepsLen(t *testing.T) {
	b := New[int](5)
	for i := 1; i <= 7; i++ {
		b.Push(i)
	}
	if b.Len() != 5 {
		t.Fatalf("Len() = %d, want 5", b.Len())
	}
	if got := b.Items(); !reflect.DeepEqual(got, []int{3, 4, 5, 6, 7}) {
		t.Fatalf("Items() = %v", got)
	}
}

func TestHiddenNonPositiveCapacity(t *testing.T) {
	b := New[int](0)
	if b.Cap() != 1 {
		t.Fatalf("Cap() = %d", b.Cap())
	}
}
