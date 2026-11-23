import time


class TokenStore:
    """Access tokens. A token is valid for ttl seconds after it was issued."""

    def __init__(self, ttl, clock=time.monotonic):
        self.ttl = ttl
        self.clock = clock
        self._issued = {}
        self._counter = 0

    def issue(self, subject):
        self._counter += 1
        token = "t%d" % self._counter
        self._issued[token] = (subject, self.clock())
        return token

    def check(self, token):
        entry = self._issued.get(token)
        if entry is None:
            return None
        subject, stamp = entry
        if self.clock() - stamp >= self.ttl:
            del self._issued[token]
            return None
        return subject

    def revoke(self, token):
        self._issued.pop(token, None)

    def purge(self):
        now = self.clock()
        dead = [k for k, (_, stamp) in self._issued.items() if now - stamp >= self.ttl]
        for key in dead:
            del self._issued[key]
        return len(dead)

    def __len__(self):
        return len(self._issued)
