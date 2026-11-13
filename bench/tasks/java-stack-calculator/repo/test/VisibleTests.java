public class VisibleTests {
    private static void check(String input, long want) {
        Rpn.Result r = Rpn.eval(input);
        if (r.isError() || r.value() != want) {
            throw new AssertionError(input + ": want " + want + ", got " + (r.isError() ? r.error() : r.value()));
        }
    }

    private static void checkError(String input) {
        if (!Rpn.eval(input).isError()) {
            throw new AssertionError(input + ": expected an error");
        }
    }

    public static void main(String[] args) {
        check("3 4 +", 7);
        check("2 3 4 * +", 14);
        check("10 2 /", 5);
        check("5 1 2 + 4 * + 3 -", 14);
        check("  8   2 -  ", 6);
        checkError("1 +");
        checkError("1 2");
        checkError("1 x +");
        System.out.println("visible tests passed");
    }
}
