/** The running account: applies events as they happen. */
public class Live {
    private long balance;
    private long version;
    private long timestamp;

    public void apply(Event event) {
        balance = event.applyTo(balance);
        version = event.version();
        timestamp = event.timestamp();
    }

    public long balance() {
        return balance;
    }

    public Snapshot snapshot() {
        return new Snapshot(version, timestamp, balance);
    }
}
