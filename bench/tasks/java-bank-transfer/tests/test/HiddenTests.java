public class HiddenTests {
    private static void eq(String what, long want, long got) {
        if (want != got) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    private static Ledger ledger() {
        Ledger ledger = new Ledger();
        ledger.add(new Account("a", "USD", 1_000));
        ledger.add(new Account("b", "USD", 0));
        ledger.add(new Account("e", "EUR", 0));
        ledger.setRate("USD", "EUR", "0.333");
        ledger.setRate("EUR", "USD", "1.15");
        return ledger;
    }

    public static void main(String[] args) {
        // The exact balance can be sent.
        Ledger l = ledger();
        if (!l.transfer("a", "b", 1_000)) {
            throw new AssertionError("sending the exact balance was refused");
        }
        eq("a drained", 0, l.get("a").balance());
        eq("b funded", 1_000, l.get("b").balance());

        // A refused transfer changes nothing.
        l = ledger();
        if (l.transfer("a", "b", 1_001)) {
            throw new AssertionError("an overdraft was accepted");
        }
        eq("a untouched", 1_000, l.get("a").balance());
        eq("b untouched", 0, l.get("b").balance());
        if (l.transfer("b", "a", 1)) {
            throw new AssertionError("a transfer from an empty account was accepted");
        }
        eq("b still empty", 0, l.get("b").balance());

        // Two transfers cannot together exceed the balance.
        l = ledger();
        if (!l.transfer("a", "b", 600) || l.transfer("a", "b", 600)) {
            throw new AssertionError("the second transfer should have been refused");
        }
        eq("a after two", 400, l.get("a").balance());

        // Conversion rounds half up to whole cents and does not drift on binary fractions.
        eq("0.01 at 0.333", 0, Ledger.convert(1, "0.333"));
        eq("0.03 at 0.333", 1, Ledger.convert(3, "0.333"));
        eq("half rounds up", 1, Ledger.convert(1, "0.5"));
        eq("1.00 at 1.15", 115, Ledger.convert(100, "1.15"));
        eq("2.00 at 1.15", 230, Ledger.convert(200, "1.15"));
        eq("1.00 at 0.29", 29, Ledger.convert(100, "0.29"));
        eq("large amount", 1_150_000_000_000L, Ledger.convert(1_000_000_000_000L, "1.15"));

        // A converted transfer credits the converted amount.
        l = ledger();
        if (!l.transfer("a", "e", 100)) {
            throw new AssertionError("a funded conversion was refused");
        }
        eq("a after conversion", 900, l.get("a").balance());
        eq("e after conversion", 33, l.get("e").balance());
        if (!l.transfer("e", "a", 20)) {
            throw new AssertionError("a funded back conversion was refused");
        }
        eq("e after back", 13, l.get("e").balance());
        eq("a after back", 923, l.get("a").balance());
        System.out.println("hidden tests passed");
    }
}
