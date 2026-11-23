//! Turns text into tokens.

use crate::eval::EvalError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Num(i64),
    Plus,
    Minus,
    Star,
    Slash,
    Caret,
    LParen,
    RParen,
}

pub fn lex(src: &str) -> Result<Vec<Token>, EvalError> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        out.push(match c {
            ' ' | '\t' => continue,
            '+' => Token::Plus,
            '-' => Token::Minus,
            '*' => Token::Star,
            '/' => Token::Slash,
            '^' => Token::Caret,
            '(' => Token::LParen,
            ')' => Token::RParen,
            '0'..='9' => {
                let start = i - 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                let n = text
                    .parse::<i64>()
                    .map_err(|_| EvalError::Syntax(format!("number too large: {text}")))?;
                Token::Num(n)
            }
            other => return Err(EvalError::Syntax(format!("unexpected '{other}'"))),
        });
    }
    Ok(out)
}
