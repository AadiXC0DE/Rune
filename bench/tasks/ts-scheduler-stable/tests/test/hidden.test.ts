import test from "node:test";
import assert from "node:assert/strict";
import { JobQueue } from "../src/queue.ts";
import type { Job } from "../src/queue.ts";

function drain(queue: JobQueue): Job[] {
  const jobs: Job[] = [];
  for (let job = queue.next(); job !== undefined; job = queue.next()) jobs.push(job);
  return jobs;
}

test("1000 jobs over 3 priorities come out by priority, then insertion order", () => {
  const queue = new JobQueue();
  const all: Job[] = [];
  for (let i = 0; i < 1000; i++) {
    const job = { id: "j" + i, priority: (i * 7 + (i >> 3)) % 3 };
    all.push(job);
    queue.add(job);
  }
  const expected = all
    .map((job, index) => ({ job, index }))
    .sort((a, b) => b.job.priority - a.job.priority || a.index - b.index)
    .map((entry) => entry.job.id);
  assert.deepEqual(drain(queue).map((job) => job.id), expected);
});

test("jobs added between takes still queue behind earlier equal ones", () => {
  const queue = new JobQueue();
  queue.add({ id: "a", priority: 1 });
  queue.add({ id: "b", priority: 1 });
  assert.equal(queue.next()?.id, "a");
  queue.add({ id: "c", priority: 1 });
  queue.add({ id: "d", priority: 2 });
  queue.add({ id: "e", priority: 1 });
  assert.deepEqual(drain(queue).map((job) => job.id), ["d", "b", "c", "e"]);
});

test("many requeued jobs return to their original places", () => {
  const queue = new JobQueue();
  for (let i = 0; i < 50; i++) queue.add({ id: "j" + i, priority: i % 2 });
  const taken: Job[] = [];
  for (let i = 0; i < 10; i++) taken.push(queue.next()!);
  for (const job of taken.reverse()) queue.requeue(job);
  const ids = drain(queue).map((job) => job.id);
  const odd = [];
  const even = [];
  for (let i = 0; i < 50; i++) (i % 2 === 1 ? odd : even).push("j" + i);
  assert.deepEqual(ids, [...odd, ...even]);
});

test("a job requeued and then followed by new jobs still runs ahead of them", () => {
  const queue = new JobQueue();
  queue.add({ id: "a", priority: 1 });
  queue.add({ id: "b", priority: 1 });
  queue.add({ id: "c", priority: 1 });
  const a = queue.next()!;
  queue.requeue({ id: a.id, priority: 1 });
  queue.add({ id: "d", priority: 1 });
  assert.deepEqual(drain(queue).map((job) => job.id), ["a", "b", "c", "d"]);
});

test("a requeue of a job that was never added is treated as a new job", () => {
  const queue = new JobQueue();
  queue.add({ id: "a", priority: 1 });
  queue.requeue({ id: "b", priority: 1 });
  queue.add({ id: "c", priority: 1 });
  assert.deepEqual(drain(queue).map((job) => job.id), ["a", "b", "c"]);
});

test("size follows adds and takes", () => {
  const queue = new JobQueue();
  for (let i = 0; i < 5; i++) queue.add({ id: "j" + i, priority: 0 });
  queue.next();
  queue.next();
  assert.equal(queue.size, 3);
});
