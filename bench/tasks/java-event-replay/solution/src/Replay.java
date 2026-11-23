import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;

/** Rebuilds an account balance from a snapshot and an event log. */
public class Replay {
    /**
     * The balance after replaying {@code log} on top of {@code snapshot}. The log
     * may be the whole history or only the part after the snapshot, and may be
     * in any order.
     */
    public static long rebuild(Snapshot snapshot, List<Event> log) {
        List<Event> ordered = new ArrayList<>(log);
        ordered.sort(Comparator.comparingLong(Event::version));
        long balance = snapshot.balance();
        for (Event event : ordered) {
            if (event.version() <= snapshot.version()) {
                continue;
            }
            balance = event.applyTo(balance);
        }
        return balance;
    }
}
