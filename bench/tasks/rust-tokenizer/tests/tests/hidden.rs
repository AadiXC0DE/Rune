use tokenizer_fixture::{tokenize, Token};

#[test]
fn unary_minus_before_parentheses_and_after_operators() {
    assert_eq!(
        tokenize("-(-1)").unwrap(),
        vec![Token::Neg, Token::LParen, Token::Neg, Token::Num(1.0), Token::RParen]
    );
    assert_eq!(
        tokenize("2--1").unwrap(),
        vec![Token::Num(2.0), Token::Op('-'), Token::Neg, Token::Num(1.0)]
    );
    assert_eq!(tokenize("-1").unwrap(), vec![Token::Neg, Token::Num(1.0)]);
    assert_eq!(
        tokenize("4*-2").unwrap(),
        vec![Token::Num(4.0), Token::Op('*'), Token::Neg, Token::Num(2.0)]
    );
}

#[test]
fn binary_minus_stays_binary() {
    assert_eq!(
        tokenize("(1)-2").unwrap(),
        vec![Token::LParen, Token::Num(1.0), Token::RParen, Token::Op('-'), Token::Num(2.0)]
    );
}

#[test]
fn decimals_without_a_leading_or_trailing_digit() {
    assert_eq!(tokenize(".5").unwrap(), vec![Token::Num(0.5)]);
    assert_eq!(tokenize("2.").unwrap(), vec![Token::Num(2.0)]);
    assert_eq!(
        tokenize("3-.5").unwrap(),
        vec![Token::Num(3.0), Token::Op('-'), Token::Num(0.5)]
    );
    assert_eq!(
        tokenize("2.*4").unwrap(),
        vec![Token::Num(2.0), Token::Op('*'), Token::Num(4.0)]
    );
}

#[test]
fn malformed_numbers_are_errors() {
    assert!(tokenize("1..2").is_err());
    assert!(tokenize("1.2.3").is_err());
    assert!(tokenize(".").is_err());
}
