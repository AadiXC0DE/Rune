package ring

// Items returns the stored items from oldest to newest.
func (b *Buffer[T]) Items() []T {
	out := make([]T, 0, b.size)
	for i := 0; i < b.size; i++ {
		out = append(out, b.items[i%len(b.items)])
	}
	return out
}

// Each calls fn on every stored item from oldest to newest and stops early
// when fn returns false.
func (b *Buffer[T]) Each(fn func(T) bool) {
	for i := 0; i < b.size; i++ {
		if !fn(b.items[i%len(b.items)]) {
			return
		}
	}
}
