import unittest

from app.sessions import SessionStore
from app.tokens import TokenStore


class FakeClock:
    def __init__(self):
        self.now = 100.0

    def __call__(self):
        return self.now


class SessionTests(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.store = SessionStore(10, clock=self.clock)

    def test_lookup_returns_the_user(self):
        sid = self.store.start("ann")
        self.assertEqual(self.store.lookup(sid), "ann")

    def test_expires_well_after_ttl(self):
        sid = self.store.start("ann")
        self.clock.now += 11
        self.assertIsNone(self.store.lookup(sid))

    def test_reading_keeps_it_alive(self):
        sid = self.store.start("ann")
        self.clock.now += 6
        self.assertEqual(self.store.lookup(sid), "ann")
        self.clock.now += 6
        self.assertEqual(self.store.lookup(sid), "ann")

    def test_end_removes_the_session(self):
        sid = self.store.start("ann")
        self.store.end(sid)
        self.assertIsNone(self.store.lookup(sid))

    def test_purge_counts_removed(self):
        self.store.start("ann")
        self.clock.now += 5
        self.store.start("bob")
        self.clock.now += 8
        self.assertEqual(self.store.purge(), 1)
        self.assertEqual(len(self.store), 1)


class TokenTests(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()
        self.store = TokenStore(30, clock=self.clock)

    def test_check_returns_the_subject(self):
        token = self.store.issue("svc")
        self.assertEqual(self.store.check(token), "svc")

    def test_expires_well_after_ttl(self):
        token = self.store.issue("svc")
        self.clock.now += 31
        self.assertIsNone(self.store.check(token))

    def test_reading_does_not_extend_life(self):
        token = self.store.issue("svc")
        self.clock.now += 20
        self.assertEqual(self.store.check(token), "svc")
        self.clock.now += 20
        self.assertIsNone(self.store.check(token))

    def test_revoke(self):
        token = self.store.issue("svc")
        self.store.revoke(token)
        self.assertIsNone(self.store.check(token))


if __name__ == "__main__":
    unittest.main()
