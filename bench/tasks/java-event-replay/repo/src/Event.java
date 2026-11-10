/**
 * One change to an account. Versions count up by one from 1 and give the
 * order of events; timestamps are wall-clock milliseconds and can repeat.
 */
public record Event(long version, long timestamp, Kind kind, long amount) {
    public enum Kind { DEPOSIT, WITHDRAW }

    public static Event deposit(long version, long timestamp, long amount) {
        return new Event(version, timestamp, Kind.DEPOSIT, amount);
    }

    public static Event withdraw(long version, long timestamp, long amount) {
        return new Event(version, timestamp, Kind.WITHDRAW, amount);
    }

    /** The balance after this event. A withdrawal larger than the balance is refused and changes nothing. */
    public long applyTo(long balance) {
        if (kind == Kind.DEPOSIT) {
            return balance + amount;
        }
        return amount > balance ? balance : balance - amount;
    }
}
