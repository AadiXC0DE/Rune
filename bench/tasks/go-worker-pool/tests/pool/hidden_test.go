package pool

import (
	"errors"
	"runtime"
	"sync/atomic"
	"testing"
	"time"
)

var errBoom = errors.New("boom")

func settle(t *testing.T, baseline int) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if runtime.NumGoroutine() <= baseline {
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("goroutines leaked: %d running, baseline %d", runtime.NumGoroutine(), baseline)
}

func run(t *testing.T, workers int, inputs []int, fn func(int) (int, error)) ([]int, error) {
	t.Helper()
	type res struct {
		v   []int
		err error
	}
	ch := make(chan res, 1)
	go func() {
		v, err := Map(workers, inputs, fn)
		ch <- res{v, err}
	}()
	select {
	case r := <-ch:
		return r.v, r.err
	case <-time.After(5 * time.Second):
		t.Fatal("Map did not return in time")
		return nil, nil
	}
}

func TestHiddenThousandItemsKeepOrder(t *testing.T) {
	inputs := make([]int, 1000)
	for i := range inputs {
		inputs[i] = i
	}
	got, err := run(t, 8, inputs, func(n int) (int, error) {
		if n%97 == 0 {
			time.Sleep(time.Millisecond)
		}
		return n + 1, nil
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 1000 {
		t.Fatalf("len = %d", len(got))
	}
	for i, v := range got {
		if v != i+1 {
			t.Fatalf("got[%d] = %d, want %d", i, v, i+1)
		}
	}
}

func TestHiddenErrorOnFirstItemStopsEarly(t *testing.T) {
	baseline := runtime.NumGoroutine()
	inputs := make([]int, 1000)
	for i := range inputs {
		inputs[i] = i
	}
	var calls int64
	got, err := run(t, 8, inputs, func(n int) (int, error) {
		atomic.AddInt64(&calls, 1)
		if n == 0 {
			return 0, errBoom
		}
		time.Sleep(2 * time.Millisecond)
		return n, nil
	})
	if !errors.Is(err, errBoom) || got != nil {
		t.Fatalf("got %v, %v", got, err)
	}
	if n := atomic.LoadInt64(&calls); n > 50 {
		t.Fatalf("fn ran %d times after the first item failed", n)
	}
	settle(t, baseline)
}

func TestHiddenErrorOnLastItem(t *testing.T) {
	baseline := runtime.NumGoroutine()
	inputs := make([]int, 300)
	for i := range inputs {
		inputs[i] = i
	}
	got, err := run(t, 8, inputs, func(n int) (int, error) {
		if n == 299 {
			return 0, errBoom
		}
		return n, nil
	})
	if !errors.Is(err, errBoom) || got != nil {
		t.Fatalf("got %d values, %v", len(got), err)
	}
	settle(t, baseline)
}

func TestHiddenManyErrorsStillOneReturn(t *testing.T) {
	baseline := runtime.NumGoroutine()
	inputs := make([]int, 100)
	for i := range inputs {
		inputs[i] = i
	}
	_, err := run(t, 4, inputs, func(n int) (int, error) { return 0, errBoom })
	if !errors.Is(err, errBoom) {
		t.Fatalf("err = %v", err)
	}
	settle(t, baseline)
}

func TestHiddenMoreWorkersThanInputs(t *testing.T) {
	got, err := run(t, 64, []int{5, 6}, func(n int) (int, error) { return n * 10, nil })
	if err != nil || len(got) != 2 || got[0] != 50 || got[1] != 60 {
		t.Fatalf("got %v, %v", got, err)
	}
}

func TestHiddenZeroWorkersStillWorks(t *testing.T) {
	got, err := run(t, 0, []int{1, 2, 3}, func(n int) (int, error) { return n, nil })
	if err != nil || len(got) != 3 || got[2] != 3 {
		t.Fatalf("got %v, %v", got, err)
	}
}
