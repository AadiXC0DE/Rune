/** The balance after every event up to and including {@code version}. */
public record Snapshot(long version, long timestamp, long balance) {
    /** The state before any event. */
    public static final Snapshot EMPTY = new Snapshot(0, 0, 0);
}
