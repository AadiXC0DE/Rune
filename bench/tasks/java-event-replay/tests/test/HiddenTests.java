import java.util.ArrayList;
import java.util.List;

public class HiddenTests {
    private static void eq(String what, long want, long got) {
        if (want != got) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    private static List<Event> history() {
        List<Event> log = new ArrayList<>();
        log.add(Event.deposit(1, 1000, 500));
        log.add(Event.withdraw(2, 1000, 200));
        log.add(Event.deposit(3, 1000, 70));
        log.add(Event.withdraw(4, 1001, 900));
        log.add(Event.deposit(5, 1001, 30));
        log.add(Event.withdraw(6, 1001, 120));
        return log;
    }

    public static void main(String[] args) {
        List<Event> log = history();
        Live live = new Live();
        for (Event e : log) {
            live.apply(e);
        }
        eq("live balance", 280, live.balance());

        // A snapshot after every possible version, replayed over the whole log.
        for (int cut = 0; cut <= log.size(); cut++) {
            Live partial = new Live();
            Snapshot snap = Snapshot.EMPTY;
            for (int i = 0; i < cut; i++) {
                partial.apply(log.get(i));
            }
            if (cut > 0) {
                snap = partial.snapshot();
            }
            eq("snapshot at version " + cut, live.balance(), Replay.rebuild(snap, log));
            eq("tail only after version " + cut, live.balance(), Replay.rebuild(snap, log.subList(cut, log.size())));
        }

        // A snapshot taken between two events that share a millisecond.
        Live mid = new Live();
        mid.apply(log.get(0));
        mid.apply(log.get(1));
        Snapshot snap = mid.snapshot();
        eq("snapshot version", 2, snap.version());
        eq("snapshot balance", 300, snap.balance());
        eq("same millisecond", live.balance(), Replay.rebuild(snap, log));

        // A snapshot at version 0 replays everything.
        eq("version zero", live.balance(), Replay.rebuild(new Snapshot(0, 0, 0), log));

        // The same events in reverse order give the same balance.
        List<Event> reversed = new ArrayList<>(log);
        java.util.Collections.reverse(reversed);
        eq("reversed log", live.balance(), Replay.rebuild(snap, reversed));

        // A snapshot at the last version replays nothing.
        Snapshot last = live.snapshot();
        eq("snapshot at the end", live.balance(), Replay.rebuild(last, log));
        System.out.println("hidden tests passed");
    }
}
