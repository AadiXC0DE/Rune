import java.math.BigDecimal;
import java.math.RoundingMode;
import java.util.HashMap;
import java.util.Map;

/** Holds accounts and moves money between them, converting between currencies. */
public class Ledger {
    private final Map<String, Account> accounts = new HashMap<>();
    private final Map<String, String> rates = new HashMap<>();

    public void add(Account account) {
        accounts.put(account.id(), account);
    }

    public Account get(String id) {
        Account account = accounts.get(id);
        if (account == null) {
            throw new IllegalArgumentException("unknown account: " + id);
        }
        return account;
    }

    /** Sets how many units of {@code to} one unit of {@code from} buys, as decimal text such as "0.92". */
    public void setRate(String from, String to, String rate) {
        rates.put(from + ">" + to, rate);
    }

    /**
     * Converts an amount in cents at a decimal rate. The result is rounded half
     * up to a whole number of cents.
     */
    public static long convert(long cents, String rate) {
        return new BigDecimal(rate).multiply(BigDecimal.valueOf(cents)).setScale(0, RoundingMode.HALF_UP).longValueExact();
    }

    /** Moves cents from one account to another. Returns false, changing nothing, when the source cannot cover it. */
    public boolean transfer(String fromId, String toId, long cents) {
        Account from = get(fromId);
        Account to = get(toId);
        long credited = cents;
        if (!from.currency().equals(to.currency())) {
            String rate = rates.get(from.currency() + ">" + to.currency());
            if (rate == null) {
                throw new IllegalStateException("no rate from " + from.currency() + " to " + to.currency());
            }
            credited = convert(cents, rate);
        }
        if (!from.debit(cents)) {
            return false;
        }
        to.credit(credited);
        return true;
    }
}
