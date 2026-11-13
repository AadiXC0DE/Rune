//! Builds an expression tree.
//!
//! Precedence, loosest first: `+ -`, then `* /`, then prefix `-`, then `^`.
//! Binary operators other than `^` group to the left; `^` groups to the right.

use crate::eval::EvalError;
use crate::lexer::Token;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Num(i64),
    Neg(Box<Expr>),
    Bin(Box<Expr>, Op, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
}

pub fn parse(tokens: &[Token]) -> Result<Expr, EvalError> {
    let mut p = Parser { tokens, pos: 0 };
    let expr = p.expr()?;
    match p.peek() {
        None => Ok(expr),
        Some(t) => Err(EvalError::Syntax(format!("unexpected {t:?}"))),
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<Token> {
        let t = self.peek();
        self.pos += 1;
        t
    }

    fn expr(&mut self) -> Result<Expr, EvalError> {
        let mut left = self.term()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => Op::Add,
                Some(Token::Minus) => Op::Sub,
                _ => return Ok(left),
            };
            self.bump();
            let right = self.term()?;
            left = Expr::Bin(Box::new(left), op, Box::new(right));
        }
    }

    fn term(&mut self) -> Result<Expr, EvalError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => Op::Mul,
                Some(Token::Slash) => Op::Div,
                _ => return Ok(left),
            };
            self.bump();
            let right = self.unary()?;
            left = Expr::Bin(Box::new(left), op, Box::new(right));
        }
    }

    fn unary(&mut self) -> Result<Expr, EvalError> {
        if self.peek() == Some(Token::Minus) {
            self.bump();
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        self.power()
    }

    fn power(&mut self) -> Result<Expr, EvalError> {
        let base = self.atom()?;
        if self.peek() == Some(Token::Caret) {
            self.bump();
            let exp = self.unary()?;
            return Ok(Expr::Bin(Box::new(base), Op::Pow, Box::new(exp)));
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<Expr, EvalError> {
        match self.bump() {
            Some(Token::Num(n)) => Ok(Expr::Num(n)),
            Some(Token::LParen) => {
                let inner = self.expr()?;
                match self.bump() {
                    Some(Token::RParen) => Ok(inner),
                    _ => Err(EvalError::Syntax("missing ')'".to_string())),
                }
            }
            Some(t) => Err(EvalError::Syntax(format!("unexpected {t:?}"))),
            None => Err(EvalError::Syntax("unexpected end of input".to_string())),
        }
    }
}
