import unittest

from app.sessions import SessionStore
from app.tokens import TokenStore


class FakeClock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now


class HiddenTests(unittest.TestCase):
    def test_session_at_exactly_ttl_is_expired(self):
        clock = FakeClock()
        store = SessionStore(10, clock=clock)
        sid = store.start("ann")
        clock.now = 10
        self.assertIsNone(store.lookup(sid))

    def test_token_at_exactly_ttl_is_expired(self):
        clock = FakeClock()
        store = TokenStore(10, clock=clock)
        token = store.issue("svc")
        clock.now = 10
        self.assertIsNone(store.check(token))

    def test_just_before_ttl_is_alive_in_both(self):
        clock = FakeClock()
        sessions = SessionStore(10, clock=clock)
        tokens = TokenStore(10, clock=clock)
        sid = sessions.start("ann")
        token = tokens.issue("svc")
        clock.now = 9.999
        self.assertEqual(sessions.lookup(sid), "ann")
        self.assertEqual(tokens.check(token), "svc")

    def test_purge_uses_the_same_boundary(self):
        clock = FakeClock()
        sessions = SessionStore(5, clock=clock)
        tokens = TokenStore(5, clock=clock)
        sessions.start("a")
        tokens.issue("a")
        clock.now = 5
        self.assertEqual(sessions.purge(), 1)
        self.assertEqual(tokens.purge(), 1)
        self.assertEqual(len(sessions), 0)
        self.assertEqual(len(tokens), 0)

    def test_sliding_session_restarts_at_each_read(self):
        clock = FakeClock()
        store = SessionStore(10, clock=clock)
        sid = store.start("ann")
        for step in range(1, 6):
            clock.now = step * 9
            self.assertEqual(store.lookup(sid), "ann")
        clock.now += 10
        self.assertIsNone(store.lookup(sid))

    def test_fixed_token_ignores_reads(self):
        clock = FakeClock()
        store = TokenStore(10, clock=clock)
        token = store.issue("svc")
        clock.now = 9
        self.assertEqual(store.check(token), "svc")
        clock.now = 10
        self.assertIsNone(store.check(token))

    def test_ids_stay_distinct_and_independent(self):
        clock = FakeClock()
        store = SessionStore(10, clock=clock)
        a, b = store.start("a"), store.start("b")
        self.assertNotEqual(a, b)
        store.end(a)
        self.assertEqual(store.lookup(b), "b")


if __name__ == "__main__":
    unittest.main()
