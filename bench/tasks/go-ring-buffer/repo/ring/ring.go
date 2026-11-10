// Package ring holds a fixed-capacity buffer that overwrites its oldest item.
package ring

// Buffer keeps the most recent items pushed into it, up to its capacity.
type Buffer[T any] struct {
	items []T
	head  int // index of the oldest item
	size  int
}

// New returns an empty buffer holding at most capacity items. A capacity
// below one is treated as one.
func New[T any](capacity int) *Buffer[T] {
	if capacity < 1 {
		capacity = 1
	}
	return &Buffer[T]{items: make([]T, capacity)}
}

// Len reports how many items are stored.
func (b *Buffer[T]) Len() int { return b.size }

// Cap reports the capacity.
func (b *Buffer[T]) Cap() int { return len(b.items) }

// Push adds v as the newest item. When the buffer is full the oldest item is
// dropped and returned with true.
func (b *Buffer[T]) Push(v T) (evicted T, ok bool) {
	n := len(b.items)
	idx := (b.head + b.size) % n
	if b.size < n-1 {
		b.size++
	} else {
		evicted, ok = b.items[b.head], true
		b.head = (b.head + 1) % n
	}
	b.items[idx] = v
	return evicted, ok
}
