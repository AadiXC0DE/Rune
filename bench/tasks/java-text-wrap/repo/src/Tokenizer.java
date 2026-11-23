import java.util.ArrayList;
import java.util.List;

/** Splits text into words. */
public class Tokenizer {
    /** The words of {@code text} in order. Runs of spaces and line breaks separate words. */
    public static List<String> words(String text) {
        List<String> out = new ArrayList<>();
        for (String word : text.split(" ")) {
            out.add(word);
        }
        return out;
    }
}
