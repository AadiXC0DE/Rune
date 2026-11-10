package pool

import (
	"errors"
	"testing"
	"time"
)

func within[T any](t *testing.T, limit time.Duration, f func() T) T {
	t.Helper()
	ch := make(chan T, 1)
	go func() { ch <- f() }()
	select {
	case v := <-ch:
		return v
	case <-time.After(limit):
		t.Fatal("Map did not return in time")
		panic("unreachable")
	}
}

type outcome struct {
	values []int
	err    error
}

func TestEmpty(t *testing.T) {
	got := within(t, 5*time.Second, func() outcome {
		v, err := Map(4, []int{}, func(n int) (int, error) { return n, nil })
		return outcome{v, err}
	})
	if got.err != nil || len(got.values) != 0 {
		t.Fatalf("got %v, %v", got.values, got.err)
	}
}

func TestLargeBatchFinishes(t *testing.T) {
	inputs := make([]int, 200)
	for i := range inputs {
		inputs[i] = i
	}
	got := within(t, 5*time.Second, func() outcome {
		v, err := Map(4, inputs, func(n int) (int, error) { return n * n, nil })
		return outcome{v, err}
	})
	if got.err != nil || len(got.values) != 200 {
		t.Fatalf("got %d values, err %v", len(got.values), got.err)
	}
	sum := 0
	for _, v := range got.values {
		sum += v
	}
	if sum != 2646700 {
		t.Fatalf("sum of squares = %d", sum)
	}
}

func TestResultsFollowInputOrder(t *testing.T) {
	inputs := []int{30, 1, 20, 2, 10, 3}
	got := within(t, 5*time.Second, func() outcome {
		v, err := Map(3, inputs, func(n int) (int, error) {
			time.Sleep(time.Duration(n) * time.Millisecond)
			return n * 2, nil
		})
		return outcome{v, err}
	})
	want := []int{60, 2, 40, 4, 20, 6}
	if got.err != nil {
		t.Fatal(got.err)
	}
	for i := range want {
		if got.values[i] != want[i] {
			t.Fatalf("values = %v, want %v", got.values, want)
		}
	}
}

func TestErrorIsReturned(t *testing.T) {
	boom := errors.New("boom")
	got := within(t, 5*time.Second, func() outcome {
		v, err := Map(2, []int{1}, func(int) (int, error) { return 0, boom })
		return outcome{v, err}
	})
	if !errors.Is(got.err, boom) || got.values != nil {
		t.Fatalf("got %v, %v", got.values, got.err)
	}
}
