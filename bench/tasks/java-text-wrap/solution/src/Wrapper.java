import java.util.ArrayList;
import java.util.List;

/** Breaks text into lines of at most a given width. */
public class Wrapper {
    /**
     * Wraps {@code text} so that no line is longer than {@code width}. Words are
     * joined by single spaces and moved whole to the next line when they do not
     * fit. A word longer than the width is split across lines at the width.
     */
    public static List<String> wrap(String text, int width) {
        if (width < 1) {
            throw new IllegalArgumentException("width must be at least 1");
        }
        List<String> lines = new ArrayList<>();
        StringBuilder line = new StringBuilder();
        for (String word : Tokenizer.words(text)) {
            if (line.length() > 0 && line.length() + 1 + word.length() <= width) {
                line.append(' ').append(word);
                continue;
            }
            if (line.length() > 0) {
                lines.add(line.toString());
                line = new StringBuilder();
            }
            while (word.length() > width) {
                lines.add(word.substring(0, width));
                word = word.substring(width);
            }
            line.append(word);
        }
        if (line.length() > 0) {
            lines.add(line.toString());
        }
        return lines;
    }
}
