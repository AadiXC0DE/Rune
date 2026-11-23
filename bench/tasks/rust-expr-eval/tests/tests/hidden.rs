use calc_fixture::{evaluate, EvalError};

#[test]
fn subtraction_and_division_group_to_the_left() {
    let cases = [
        ("2-3-4", -5),
        ("10-4-3", 3),
        ("10-4+3", 9),
        ("100/10/5", 2),
        ("100/10*5", 50),
        ("8/4/2", 1),
        ("2*3-4*5-1", -15),
        ("1-(2-3)", 2),
    ];
    for (src, want) in cases {
        assert_eq!(evaluate(src), Ok(want), "{src}");
    }
}

#[test]
fn power_groups_to_the_right_and_binds_tighter_than_minus() {
    let cases = [
        ("2^3^2", 512),
        ("2^2^3", 256),
        ("-2^2", -4),
        ("(-2)^2", 4),
        ("2*3^2", 18),
        ("2^-0", 1),
        ("0^0", 1),
        ("--3", 3),
    ];
    for (src, want) in cases {
        assert_eq!(evaluate(src), Ok(want), "{src}");
    }
}

#[test]
fn division_by_zero_is_an_error() {
    for src in ["1/0", "0/0", "5/(3-3)", "2*(1/0)", "1+7/(2-2)"] {
        assert_eq!(evaluate(src), Err(EvalError::DivisionByZero), "{src}");
    }
}

#[test]
fn overflow_is_an_error() {
    let cases = [
        "9223372036854775807+1",
        "-9223372036854775807-2",
        "4611686018427387904*2",
        "-(-9223372036854775807-1)",
        "(-9223372036854775807-1)/-1",
        "2^63",
        "2^64",
        "3^40",
        "2^4294967296",
    ];
    for src in cases {
        assert_eq!(evaluate(src), Err(EvalError::Overflow), "{src}");
    }
}

#[test]
fn values_at_the_limits_still_work() {
    assert_eq!(evaluate("9223372036854775807"), Ok(i64::MAX));
    assert_eq!(evaluate("-9223372036854775807-1"), Ok(i64::MIN));
    assert_eq!(evaluate("9223372036854775806+1"), Ok(i64::MAX));
    assert_eq!(evaluate("2^62"), Ok(1 << 62));
    assert_eq!(evaluate("(-2)^63"), Ok(i64::MIN));
}

#[test]
fn bad_input_is_reported_not_panicked() {
    assert!(matches!(evaluate("99999999999999999999"), Err(EvalError::Syntax(_))));
    assert!(matches!(evaluate("2^-1"), Err(EvalError::NegativeExponent)));
    assert!(matches!(evaluate("1 2"), Err(EvalError::Syntax(_))));
    assert!(matches!(evaluate("()"), Err(EvalError::Syntax(_))));
    assert!(matches!(evaluate("*3"), Err(EvalError::Syntax(_))));
}
