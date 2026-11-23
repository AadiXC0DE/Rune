export interface Job {
  id: string;
  priority: number;
}

/**
 * A queue of jobs. `next()` returns the job with the highest priority; jobs
 * with the same priority come out in the order they were added. A job taken
 * out earlier can be put back with `requeue()` and then keeps the place it
 * had among jobs of its priority.
 */
export class JobQueue {
  private heap: Job[] = [];

  get size(): number {
    return this.heap.length;
  }

  add(job: Job): void {
    this.push(job);
  }

  requeue(job: Job): void {
    this.push(job);
  }

  next(): Job | undefined {
    const top = this.heap[0];
    const last = this.heap.pop();
    if (last !== undefined && this.heap.length > 0) {
      this.heap[0] = last;
      this.siftDown(0);
    }
    return top;
  }

  private push(job: Job): void {
    this.heap.push(job);
    this.siftUp(this.heap.length - 1);
  }

  private siftUp(index: number): void {
    let child = index;
    while (child > 0) {
      const parent = (child - 1) >> 1;
      if (this.heap[child].priority < this.heap[parent].priority) break;
      this.swap(child, parent);
      child = parent;
    }
  }

  private siftDown(index: number): void {
    let parent = index;
    for (;;) {
      const left = parent * 2 + 1;
      const right = left + 1;
      let best = parent;
      if (left < this.heap.length && this.heap[left].priority > this.heap[best].priority) best = left;
      if (right < this.heap.length && this.heap[right].priority > this.heap[best].priority) best = right;
      if (best === parent) return;
      this.swap(parent, best);
      parent = best;
    }
  }

  private swap(a: number, b: number): void {
    const held = this.heap[a];
    this.heap[a] = this.heap[b];
    this.heap[b] = held;
  }
}
