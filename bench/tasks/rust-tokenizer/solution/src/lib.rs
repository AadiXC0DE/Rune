//! A tokenizer for arithmetic expressions.

/// One lexical unit of an expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Num(f64),
    /// A binary operator: one of `+ - * /`.
    Op(char),
    /// A prefix minus.
    Neg,
    LParen,
    RParen,
}

/// Splits an expression into tokens. A `-` that has no left operand (at the
/// start, after an operator, after another prefix minus or after `(`) is
/// `Neg`; every other `-` is `Op('-')`.
pub fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut out: Vec<Token> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '0'..='9' | '.' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                let value = text.parse::<f64>().map_err(|_| format!("bad number '{text}'"))?;
                out.push(Token::Num(value));
            }
            '-' => {
                let unary = matches!(
                    out.last(),
                    None | Some(Token::Op(_)) | Some(Token::Neg) | Some(Token::LParen)
                );
                out.push(if unary { Token::Neg } else { Token::Op('-') });
                i += 1;
            }
            '+' | '*' | '/' => {
                out.push(Token::Op(c));
                i += 1;
            }
            '(' => {
                out.push(Token::LParen);
                i += 1;
            }
            ')' => {
                out.push(Token::RParen);
                i += 1;
            }
            other => return Err(format!("unexpected '{other}'")),
        }
    }
    Ok(out)
}
