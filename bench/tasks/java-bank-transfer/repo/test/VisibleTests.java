public class VisibleTests {
    private static void eq(String what, long want, long got) {
        if (want != got) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    public static void main(String[] args) {
        Ledger ledger = new Ledger();
        ledger.add(new Account("a", "USD", 10_000));
        ledger.add(new Account("b", "USD", 500));
        ledger.add(new Account("c", "EUR", 0));
        ledger.setRate("USD", "EUR", "0.5");

        if (!ledger.transfer("a", "b", 2_500)) {
            throw new AssertionError("a funded transfer was refused");
        }
        eq("a after", 7_500, ledger.get("a").balance());
        eq("b after", 3_000, ledger.get("b").balance());

        if (!ledger.transfer("a", "c", 1_000)) {
            throw new AssertionError("a funded conversion was refused");
        }
        eq("c after", 500, ledger.get("c").balance());

        if (ledger.transfer("b", "a", 9_999_999)) {
            throw new AssertionError("an unfunded transfer was accepted");
        }
        eq("convert", 2_000, Ledger.convert(1_000, "2.0"));
        System.out.println("visible tests passed");
    }
}
