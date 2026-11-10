import time


class SessionStore:
    """Login sessions. Reading a session keeps it alive for another ttl seconds."""

    def __init__(self, ttl, clock=time.monotonic):
        self.ttl = ttl
        self.clock = clock
        self._entries = {}
        self._counter = 0

    def start(self, user):
        self._counter += 1
        sid = "s%d" % self._counter
        self._entries[sid] = (user, self.clock())
        return sid

    def lookup(self, sid):
        entry = self._entries.get(sid)
        if entry is None:
            return None
        user, stamp = entry
        now = self.clock()
        if now - stamp > self.ttl:
            del self._entries[sid]
            return None
        self._entries[sid] = (user, now)
        return user

    def end(self, sid):
        self._entries.pop(sid, None)

    def purge(self):
        now = self.clock()
        dead = [k for k, (_, stamp) in self._entries.items() if now - stamp > self.ttl]
        for key in dead:
            del self._entries[key]
        return len(dead)

    def __len__(self):
        return len(self._entries)
