/**
 * A small event emitter. Listeners run in the order they were added.
 * `once` listeners run for a single emit and are then removed.
 */
class Emitter {
  constructor() {
    this.handlers = new Map();
  }

  on(event, listener) {
    if (!this.handlers.has(event)) this.handlers.set(event, []);
    this.handlers.get(event).push(listener);
    return this;
  }

  once(event, listener) {
    const wrapper = (...args) => {
      this.off(event, wrapper);
      listener(...args);
    };
    wrapper.original = listener;
    return this.on(event, wrapper);
  }

  off(event, listener) {
    const list = this.handlers.get(event);
    if (!list) return this;
    const index = list.findIndex((item) => item === listener || item.original === listener);
    if (index !== -1) list.splice(index, 1);
    return this;
  }

  emit(event, ...args) {
    const list = this.handlers.get(event);
    if (!list || list.length === 0) return false;
    for (const listener of [...list]) {
      listener(...args);
    }
    return true;
  }

  listenerCount(event) {
    const list = this.handlers.get(event);
    return list ? list.length : 0;
  }
}

module.exports = { Emitter };
