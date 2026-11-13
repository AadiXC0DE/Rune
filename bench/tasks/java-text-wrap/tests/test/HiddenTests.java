import java.util.Arrays;
import java.util.List;

public class HiddenTests {
    private static void check(String text, int width, String... want) {
        List<String> got = Wrapper.wrap(text, width);
        if (!got.equals(Arrays.asList(want))) {
            throw new AssertionError("wrap(\"" + text + "\", " + width + "): want " + Arrays.asList(want) + ", got " + got);
        }
    }

    public static void main(String[] args) {
        // A word moves whole to the next line when it does not fit.
        check("aaa bbb ccc", 5, "aaa", "bbb", "ccc");
        check("aaa bbb ccc", 7, "aaa bbb", "ccc");
        check("hello wonderful world", 10, "hello", "wonderful", "world");

        // Width 1 puts one character on each line.
        check("ab c", 1, "a", "b", "c");

        // A word exactly as wide as the line fits; one more character does not.
        check("abcde", 5, "abcde");
        check("abcde fg", 5, "abcde", "fg");
        check("ab cde", 5, "ab", "cde");
        check("ab cd", 5, "ab cd");

        // Longer words are kept and split at the width.
        check("abcdefghij", 4, "abcd", "efgh", "ij");
        check("hi abcdefg ok", 4, "hi", "abcd", "efg", "ok");
        check("abcdefg x", 4, "abcd", "efg", "x");

        // Runs of whitespace collapse and edges are trimmed.
        check("a    b", 10, "a b");
        check("  a  b  ", 10, "a b");
        check("a\n\nb c", 10, "a b c");
        check("   ", 5);
        check("one  two   three", 8, "one two", "three");
        System.out.println("hidden tests passed");
    }
}
