export interface Job {
  id: string;
  priority: number;
}

interface Entry {
  job: Job;
  seq: number;
}

/**
 * A queue of jobs. `next()` returns the job with the highest priority; jobs
 * with the same priority come out in the order they were added. A job taken
 * out earlier can be put back with `requeue()` and then keeps the place it
 * had among jobs of its priority.
 */
export class JobQueue {
  private heap: Entry[] = [];
  private seqs = new Map<string, number>();
  private counter = 0;

  get size(): number {
    return this.heap.length;
  }

  add(job: Job): void {
    const seq = this.counter++;
    this.seqs.set(job.id, seq);
    this.push({ job, seq });
  }

  requeue(job: Job): void {
    let seq = this.seqs.get(job.id);
    if (seq === undefined) {
      seq = this.counter++;
      this.seqs.set(job.id, seq);
    }
    this.push({ job, seq });
  }

  next(): Job | undefined {
    const top = this.heap[0];
    const last = this.heap.pop();
    if (last !== undefined && this.heap.length > 0) {
      this.heap[0] = last;
      this.siftDown(0);
    }
    return top?.job;
  }

  private before(a: Entry, b: Entry): boolean {
    if (a.job.priority !== b.job.priority) return a.job.priority > b.job.priority;
    return a.seq < b.seq;
  }

  private push(entry: Entry): void {
    this.heap.push(entry);
    this.siftUp(this.heap.length - 1);
  }

  private siftUp(index: number): void {
    let child = index;
    while (child > 0) {
      const parent = (child - 1) >> 1;
      if (!this.before(this.heap[child], this.heap[parent])) break;
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
      if (left < this.heap.length && this.before(this.heap[left], this.heap[best])) best = left;
      if (right < this.heap.length && this.before(this.heap[right], this.heap[best])) best = right;
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
