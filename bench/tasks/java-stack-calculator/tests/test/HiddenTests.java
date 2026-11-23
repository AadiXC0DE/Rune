public class HiddenTests {
    private static void check(String input, long want) {
        Rpn.Result r = Rpn.eval(input);
        if (r.isError() || r.value() != want) {
            throw new AssertionError(input + ": want " + want + ", got " + (r.isError() ? r.error() : r.value()));
        }
    }

    private static void checkError(String input) {
        Rpn.Result r;
        try {
            r = Rpn.eval(input);
        } catch (RuntimeException e) {
            throw new AssertionError("'" + input + "' threw " + e);
        }
        if (!r.isError()) {
            throw new AssertionError("'" + input + "': expected an error, got " + r.value());
        }
    }

    public static void main(String[] args) {
        check("-3 4 +", 1);
        check("4 -3 +", 1);
        check("-3 -4 *", 12);
        check("-12 -4 /", 3);
        check("5 -2 -", 7);
        check("-7", -7);
        check("0 -5 -", 5);
        checkError("1 0 /");
        checkError("0 0 /");
        checkError("-4 2 2 - /");
        checkError("");
        checkError("   ");
        checkError("-");
        checkError("--3 1 +");
        System.out.println("hidden tests passed");
    }
}
