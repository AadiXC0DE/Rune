import time

from .ttl import TTLMap


class SessionStore:
    """Login sessions. Reading a session keeps it alive for another ttl seconds."""

    def __init__(self, ttl, clock=time.monotonic):
        self._map = TTLMap(ttl, clock, sliding=True)
        self._counter = 0

    @property
    def ttl(self):
        return self._map.ttl

    def start(self, user):
        self._counter += 1
        sid = "s%d" % self._counter
        self._map.put(sid, user)
        return sid

    def lookup(self, sid):
        return self._map.get(sid)

    def end(self, sid):
        self._map.delete(sid)

    def purge(self):
        return self._map.purge()

    def __len__(self):
        return len(self._map)
