/** An account holding a balance in whole cents of one currency. */
public class Account {
    private final String id;
    private final String currency;
    private long balance;

    public Account(String id, String currency, long balance) {
        this.id = id;
        this.currency = currency;
        this.balance = balance;
    }

    public String id() {
        return id;
    }

    public String currency() {
        return currency;
    }

    public long balance() {
        return balance;
    }

    public void credit(long cents) {
        balance += cents;
    }

    /** Removes cents from the account. Returns false when the balance cannot cover them. */
    public boolean debit(long cents) {
        balance -= cents;
        if (balance < 0) {
            return false;
        }
        return true;
    }
}
