//! An integer calculator: `+ - * / ^`, unary minus and parentheses.

pub mod eval;
pub mod lexer;
pub mod parser;

pub use eval::{evaluate, EvalError};
