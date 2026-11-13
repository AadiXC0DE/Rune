//! Evaluates an expression tree with 64-bit signed integers.

use crate::lexer::lex;
use crate::parser::{parse, Expr, Op};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    Syntax(String),
    DivisionByZero,
    Overflow,
    NegativeExponent,
}

/// Parses and evaluates `src`. Division truncates toward zero.
pub fn evaluate(src: &str) -> Result<i64, EvalError> {
    let tokens = lex(src)?;
    if tokens.is_empty() {
        return Err(EvalError::Syntax("empty expression".to_string()));
    }
    let tree = parse(&tokens)?;
    eval(&tree)
}

fn eval(expr: &Expr) -> Result<i64, EvalError> {
    match expr {
        Expr::Num(n) => Ok(*n),
        Expr::Neg(inner) => Ok(-eval(inner)?),
        Expr::Bin(l, op, r) => {
            let (a, b) = (eval(l)?, eval(r)?);
            match op {
                Op::Add => Ok(a + b),
                Op::Sub => Ok(a - b),
                Op::Mul => Ok(a * b),
                Op::Div => Ok(a / b),
                Op::Pow => {
                    if b < 0 {
                        return Err(EvalError::NegativeExponent);
                    }
                    Ok(a.pow(b as u32))
                }
            }
        }
    }
}
