import test from "node:test";
import assert from "node:assert/strict";
import { JobQueue } from "../src/queue.ts";

function drain(queue: JobQueue): string[] {
  const ids: string[] = [];
  for (let job = queue.next(); job !== undefined; job = queue.next()) ids.push(job.id);
  return ids;
}

test("higher priority comes out first", () => {
  const queue = new JobQueue();
  queue.add({ id: "low", priority: 1 });
  queue.add({ id: "high", priority: 9 });
  queue.add({ id: "mid", priority: 5 });
  assert.deepEqual(drain(queue), ["high", "mid", "low"]);
});

test("an empty queue returns undefined", () => {
  const queue = new JobQueue();
  assert.equal(queue.next(), undefined);
  assert.equal(queue.size, 0);
});

test("jobs of equal priority run in the order they were added", () => {
  const queue = new JobQueue();
  const ids = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
  for (const id of ids) queue.add({ id, priority: 3 });
  assert.deepEqual(drain(queue), ids);
});

test("a requeued job keeps its original position", () => {
  const queue = new JobQueue();
  queue.add({ id: "a", priority: 2 });
  const first = queue.next();
  assert.equal(first?.id, "a");
  queue.add({ id: "b", priority: 2 });
  queue.requeue(first!);
  assert.deepEqual(drain(queue), ["a", "b"]);
});
