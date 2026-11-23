// Package pool runs a function over a batch of inputs on several goroutines.
package pool

import "sync"

// Map applies fn to every input using at most workers goroutines. The
// outputs are in the order of the inputs. If fn fails, Map stops handing out
// work and returns the error.
func Map[In, Out any](workers int, inputs []In, fn func(In) (Out, error)) ([]Out, error) {
	if len(inputs) == 0 {
		return nil, nil
	}
	if workers < 1 {
		workers = 1
	}
	if workers > len(inputs) {
		workers = len(inputs)
	}
	outs := make([]Out, len(inputs))
	jobs := make(chan int)
	done := make(chan struct{})
	var once sync.Once
	var firstErr error
	fail := func(err error) {
		once.Do(func() {
			firstErr = err
			close(done)
		})
	}
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		defer close(jobs)
		for i := range inputs {
			select {
			case jobs <- i:
			case <-done:
				return
			}
		}
	}()
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := range jobs {
				select {
				case <-done:
					return
				default:
				}
				out, err := fn(inputs[i])
				if err != nil {
					fail(err)
					return
				}
				outs[i] = out
			}
		}()
	}
	wg.Wait()
	if firstErr != nil {
		return nil, firstErr
	}
	return outs, nil
}
