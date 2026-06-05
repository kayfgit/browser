//! A tiny arithmetic evaluator for the command bar's "quick maths" feature.
//!
//! Supports `+ - * / % ^`, parentheses, unary `+`/`-`, and decimal numbers, with
//! the usual precedence (`^` is right-associative and binds tightest; unary minus
//! sits just below it so `-2^2 == -4`). Returns `None` for anything that isn't a
//! complete, finite expression, so the caller can tell "this is maths" from "this
//! is a command".

/// Evaluate an arithmetic expression. Returns `None` if it doesn't fully parse,
/// divides by zero, or overflows to a non-finite value.
pub fn eval(input: &str) -> Option<f64> {
    let tokens = tokenize(input)?;
    let mut p = Parser { toks: &tokens, pos: 0 };
    let value = p.expr(0)?;
    // Reject trailing garbage like "2 3" or "2 +".
    if p.pos != tokens.len() {
        return None;
    }
    value.is_finite().then_some(value)
}

#[derive(Clone, Copy, PartialEq)]
enum Tok {
    Num(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    LParen,
    RParen,
}

fn tokenize(s: &str) -> Option<Vec<Tok>> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' => i += 1,
            b'+' => {
                out.push(Tok::Plus);
                i += 1;
            }
            b'-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            b'*' => {
                out.push(Tok::Star);
                i += 1;
            }
            b'/' => {
                out.push(Tok::Slash);
                i += 1;
            }
            b'%' => {
                out.push(Tok::Percent);
                i += 1;
            }
            b'^' => {
                out.push(Tok::Caret);
                i += 1;
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b'0'..=b'9' | b'.' => {
                let start = i;
                let mut dots = 0;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    if bytes[i] == b'.' {
                        dots += 1;
                    }
                    i += 1;
                }
                if dots > 1 {
                    return None;
                }
                let num: f64 = s[start..i].parse().ok()?;
                out.push(Tok::Num(num));
            }
            // Any other byte (a letter, etc.) means this isn't a maths expression.
            _ => return None,
        }
    }
    (!out.is_empty()).then_some(out)
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<Tok> {
        self.toks.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.peek();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// Pratt parser: parse an expression whose operators bind at least `min_bp`.
    fn expr(&mut self, min_bp: u8) -> Option<f64> {
        // Prefix: a number, a parenthesized expression, or a unary +/-.
        let mut lhs = match self.bump()? {
            Tok::Num(n) => n,
            Tok::LParen => {
                let v = self.expr(0)?;
                if self.bump()? != Tok::RParen {
                    return None;
                }
                v
            }
            // Unary minus/plus bind just under `^` (lbp 7) so `-2^2 == -4`.
            Tok::Minus => -self.expr(6)?,
            Tok::Plus => self.expr(6)?,
            _ => return None,
        };
        // Infix operators with (left, right) binding powers.
        loop {
            let (lbp, rbp, op) = match self.peek() {
                Some(Tok::Plus) => (1, 2, Tok::Plus),
                Some(Tok::Minus) => (1, 2, Tok::Minus),
                Some(Tok::Star) => (3, 4, Tok::Star),
                Some(Tok::Slash) => (3, 4, Tok::Slash),
                Some(Tok::Percent) => (3, 4, Tok::Percent),
                Some(Tok::Caret) => (7, 6, Tok::Caret), // right-associative
                _ => break,
            };
            if lbp < min_bp {
                break;
            }
            self.pos += 1;
            let rhs = self.expr(rbp)?;
            lhs = apply(op, lhs, rhs)?;
        }
        Some(lhs)
    }
}

fn apply(op: Tok, a: f64, b: f64) -> Option<f64> {
    Some(match op {
        Tok::Plus => a + b,
        Tok::Minus => a - b,
        Tok::Star => a * b,
        Tok::Slash if b == 0.0 => return None,
        Tok::Slash => a / b,
        Tok::Percent if b == 0.0 => return None,
        Tok::Percent => a % b,
        Tok::Caret => a.powf(b),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert!(approx(eval("20*8").unwrap(), 160.0));
        assert!(approx(eval("160+10").unwrap(), 170.0));
        assert!(approx(eval("2+3*4").unwrap(), 14.0));
        assert!(approx(eval("(2+3)*4").unwrap(), 20.0));
        assert!(approx(eval("10/4").unwrap(), 2.5));
        assert!(approx(eval("2^10").unwrap(), 1024.0));
        assert!(approx(eval("-2^2").unwrap(), -4.0));
        assert!(approx(eval("17 % 5").unwrap(), 2.0));
    }

    #[test]
    fn rejects_non_maths_and_errors() {
        assert_eq!(eval("open youtube.com"), None);
        assert_eq!(eval("rustlang"), None);
        assert_eq!(eval("2 +"), None);
        assert_eq!(eval("1/0"), None);
        assert_eq!(eval(""), None);
        assert_eq!(eval("1..2"), None);
    }
}
