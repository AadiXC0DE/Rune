import time

from .ttl import TTLMap


class TokenStore:
    """Access tokens. A token is valid for ttl seconds after it was issued."""

    def __init__(self, ttl, clock=time.monotonic):
        self._map = TTLMap(ttl, clock)
        self._counter = 0

    @property
    def ttl(self):
        return self._map.ttl

    def issue(self, subject):
        self._counter += 1
        token = "t%d" % self._counter
        self._map.put(token, subject)
        return token

    def check(self, token):
        return self._map.get(token)

    def revoke(self, token):
        self._map.delete(token)

    def purge(self):
        return self._map.purge()

    def __len__(self):
        return len(self._map)
