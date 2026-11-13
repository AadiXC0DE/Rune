import java.util.List;

public class VisibleTests {
    private static void eq(String what, long want, long got) {
        if (want != got) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    public static void main(String[] args) {
        // Without a snapshot every event counts.
        List<Event> all = List.of(Event.deposit(1, 100, 50), Event.withdraw(2, 110, 20), Event.deposit(3, 120, 5));
        eq("no snapshot", 35, Replay.rebuild(Snapshot.EMPTY, all));

        // Withdrawals beyond the balance are refused, as in the live account.
        eq("refused withdrawal", 10, Replay.rebuild(Snapshot.EMPTY, List.of(Event.deposit(1, 1, 10), Event.withdraw(2, 2, 11))));

        // A log holding only the events after the snapshot, several in the same millisecond as it.
        Snapshot snap = new Snapshot(2, 1000, 50);
        List<Event> tail = List.of(Event.deposit(3, 1000, 10), Event.withdraw(4, 1000, 5), Event.deposit(5, 1000, 1));
        eq("tail with equal timestamps", 56, Replay.rebuild(snap, tail));

        // The log may arrive out of order.
        eq("unordered", 56, Replay.rebuild(snap, List.of(tail.get(2), tail.get(0), tail.get(1))));
        System.out.println("visible tests passed");
    }
}
