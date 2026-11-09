/**
 * A cache holding at most `capacity` entries. Reading or writing a key makes
 * it the most recently used; inserting past capacity evicts the least
 * recently used entry.
 */
class LRUCache {
  constructor(capacity) {
    if (!Number.isInteger(capacity) || capacity < 1) {
      throw new RangeError("capacity must be a positive integer");
    }
    this.capacity = capacity;
    this.entries = new Map();
  }

  get(key) {
    if (!this.entries.has(key)) return undefined;
    const value = this.entries.get(key);
    this.entries.delete(key);
    this.entries.set(key, value);
    return value;
  }

  set(key, value) {
    this.entries.delete(key);
    this.entries.set(key, value);
    if (this.entries.size > this.capacity) {
      this.entries.delete(this.entries.keys().next().value);
    }
  }

  delete(key) {
    return this.entries.delete(key);
  }

  get size() {
    return this.entries.size;
  }

  keys() {
    return [...this.entries.keys()];
  }
}

module.exports = { LRUCache };
