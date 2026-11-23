import java.util.ArrayDeque;
import java.util.Deque;

/** Evaluates reverse Polish notation over whole numbers, for example "3 4 + 2 *". */
public class Rpn {
    /** The outcome of an evaluation: a value, or an error message. */
    public static final class Result {
        private final Long value;
        private final String error;

        private Result(Long value, String error) {
            this.value = value;
            this.error = error;
        }

        public static Result ok(long value) {
            return new Result(value, null);
        }

        public static Result error(String message) {
            return new Result(null, message);
        }

        public boolean isError() {
            return error != null;
        }

        public long value() {
            if (value == null) {
                throw new IllegalStateException("no value: " + error);
            }
            return value;
        }

        public String error() {
            return error;
        }
    }

    private static boolean isOperator(String token) {
        return token.equals("+") || token.equals("-") || token.equals("*") || token.equals("/");
    }

    public static Result eval(String input) {
        Deque<Long> stack = new ArrayDeque<>();
        for (String token : input.trim().split("\\s+")) {
            if (token.isEmpty()) {
                continue;
            }
            if (isOperator(token)) {
                if (stack.size() < 2) {
                    return Result.error("not enough operands for " + token);
                }
                long b = stack.pop();
                long a = stack.pop();
                switch (token.charAt(0)) {
                    case '+': stack.push(a + b); break;
                    case '-': stack.push(a - b); break;
                    case '*': stack.push(a * b); break;
                    default:
                        if (b == 0) {
                            return Result.error("division by zero");
                        }
                        stack.push(a / b);
                        break;
                }
            } else {
                try {
                    stack.push(Long.parseLong(token));
                } catch (NumberFormatException e) {
                    return Result.error("bad token: " + token);
                }
            }
        }
        if (stack.isEmpty()) {
            return Result.error("empty expression");
        }
        if (stack.size() > 1) {
            return Result.error("too many operands");
        }
        return Result.ok(stack.pop());
    }
}
