import time


class TTLMap:
    """Key to value map whose entries expire ttl seconds after they were stored.

    An entry is expired once its age reaches ttl. With sliding set, a successful
    read restarts the age.
    """

    def __init__(self, ttl, clock=time.monotonic, sliding=False):
        self.ttl = ttl
        self.clock = clock
        self.sliding = sliding
        self._entries = {}

    def _expired(self, stamp, now):
        return now - stamp >= self.ttl

    def put(self, key, value):
        self._entries[key] = (value, self.clock())

    def get(self, key):
        entry = self._entries.get(key)
        if entry is None:
            return None
        value, stamp = entry
        now = self.clock()
        if self._expired(stamp, now):
            del self._entries[key]
            return None
        if self.sliding:
            self._entries[key] = (value, now)
        return value

    def delete(self, key):
        self._entries.pop(key, None)

    def purge(self):
        now = self.clock()
        dead = [k for k, (_, stamp) in self._entries.items() if self._expired(stamp, now)]
        for key in dead:
            del self._entries[key]
        return len(dead)

    def __len__(self):
        return len(self._entries)
