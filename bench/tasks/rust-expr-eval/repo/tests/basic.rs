use calc_fixture::{evaluate, EvalError};

#[test]
fn precedence_of_multiplication_over_addition() {
    assert_eq!(evaluate("1 + 2 * 3"), Ok(7));
    assert_eq!(evaluate("(1 + 2) * 3"), Ok(9));
}

#[test]
fn powers_and_unary_minus() {
    assert_eq!(evaluate("2 ^ 10"), Ok(1024));
    assert_eq!(evaluate("-3 + 5"), Ok(2));
    assert_eq!(evaluate("-2 ^ 2"), Ok(-4));
}

#[test]
fn integer_division_truncates() {
    assert_eq!(evaluate("7 / 2"), Ok(3));
    assert_eq!(evaluate("-7 / 2"), Ok(-3));
}

#[test]
fn syntax_errors_are_reported() {
    assert!(matches!(evaluate("1 +"), Err(EvalError::Syntax(_))));
    assert!(matches!(evaluate("(1"), Err(EvalError::Syntax(_))));
    assert!(matches!(evaluate(""), Err(EvalError::Syntax(_))));
    assert!(matches!(evaluate("1 $ 2"), Err(EvalError::Syntax(_))));
}
