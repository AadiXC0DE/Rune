use tokenizer_fixture::{tokenize, Token};

#[test]
fn splits_numbers_and_operators() {
    assert_eq!(
        tokenize("1 + 22").unwrap(),
        vec![Token::Num(1.0), Token::Op('+'), Token::Num(22.0)]
    );
}

#[test]
fn reads_parentheses_and_binary_minus() {
    assert_eq!(
        tokenize("3*(4-1)").unwrap(),
        vec![
            Token::Num(3.0),
            Token::Op('*'),
            Token::LParen,
            Token::Num(4.0),
            Token::Op('-'),
            Token::Num(1.0),
            Token::RParen
        ]
    );
}

#[test]
fn reads_decimals_with_a_leading_digit() {
    assert_eq!(tokenize("0.25").unwrap(), vec![Token::Num(0.25)]);
}

#[test]
fn rejects_unknown_characters() {
    assert!(tokenize("2 $ 3").is_err());
}
