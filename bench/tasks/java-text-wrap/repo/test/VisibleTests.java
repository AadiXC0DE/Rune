import java.util.Arrays;
import java.util.List;

public class VisibleTests {
    private static void check(String text, int width, String... want) {
        List<String> got = Wrapper.wrap(text, width);
        if (!got.equals(Arrays.asList(want))) {
            throw new AssertionError("wrap(\"" + text + "\", " + width + "): want " + Arrays.asList(want) + ", got " + got);
        }
    }

    public static void main(String[] args) {
        check("the quick brown fox", 10, "the quick", "brown fox");
        check("short", 20, "short");
        check("one two three", 40, "one two three");
        check("", 10);
        check("alpha beta gamma delta", 11, "alpha beta", "gamma delta");
        System.out.println("visible tests passed");
    }
}
