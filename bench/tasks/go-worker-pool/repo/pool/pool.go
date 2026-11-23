// Package pool runs a function over a batch of inputs on several goroutines.
package pool

import "sync"

type result[Out any] struct {
	value Out
	err   error
}

// Map applies fn to every input using at most workers goroutines. The
// outputs are in the order of the inputs. If fn fails, Map stops handing out
// work and returns the error.
func Map[In, Out any](workers int, inputs []In, fn func(In) (Out, error)) ([]Out, error) {
	if workers < 1 {
		workers = 1
	}
	jobs := make(chan In)
	results := make(chan result[Out])
	var wg sync.WaitGroup
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for in := range jobs {
				out, err := fn(in)
				results <- result[Out]{value: out, err: err}
			}
		}()
	}
	for _, in := range inputs {
		jobs <- in
	}
	close(jobs)
	go func() {
		wg.Wait()
		close(results)
	}()
	var outs []Out
	for r := range results {
		if r.err != nil {
			return nil, r.err
		}
		outs = append(outs, r.value)
	}
	return outs, nil
}
