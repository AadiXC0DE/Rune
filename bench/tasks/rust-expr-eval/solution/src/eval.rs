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
        Expr::Neg(inner) => eval(inner)?.checked_neg().ok_or(EvalError::Overflow),
        Expr::Bin(l, op, r) => {
            let (a, b) = (eval(l)?, eval(r)?);
            match op {
                Op::Add => a.checked_add(b).ok_or(EvalError::Overflow),
                Op::Sub => a.checked_sub(b).ok_or(EvalError::Overflow),
                Op::Mul => a.checked_mul(b).ok_or(EvalError::Overflow),
                Op::Div => {
                    if b == 0 {
                        return Err(EvalError::DivisionByZero);
                    }
                    a.checked_div(b).ok_or(EvalError::Overflow)
                }
                Op::Pow => {
                    if b < 0 {
                        return Err(EvalError::NegativeExponent);
                    }
                    let exp = u32::try_from(b).map_err(|_| EvalError::Overflow)?;
                    a.checked_pow(exp).ok_or(EvalError::Overflow)
                }
            }
        }
    }
}
