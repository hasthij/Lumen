//! Calculator: + - * / % ^, parentheses, functions (sqrt, sin, cos, tan, asin, acos, atan,
//! ln, log, abs, round, floor, ceil, exp), constants pi and e, implicit "2pi".

use crate::launcher::item::{Action, Item, Kind};

pub fn query(q: &str, explicit: bool, out: &mut Vec<Item>) {
    let q = q.trim();
    if q.is_empty() {
        return;
    }
    // Without the "=" keyword, only answer things that are clearly maths.
    if !explicit && !(q.chars().any(|c| c.is_ascii_digit()) && q.chars().any(|c| "+-*/^%(".contains(c)) || is_fn_call(q)) {
        return;
    }
    let Some(v) = eval(q) else { return };
    let text = format_number(v);
    out.push(
        Item::new(Kind::Calculator, format!("= {text}"), format!("{q}  ·  Enter to copy"), Action::Copy(text.clone()))
            .key("calc")
            .score(5000),
    );
}

fn is_fn_call(q: &str) -> bool {
    let l = q.to_ascii_lowercase();
    ["sqrt(", "sin(", "cos(", "tan(", "log(", "ln(", "abs("].iter().any(|f| l.starts_with(f))
}

pub fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{:.10}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

pub fn eval(src: &str) -> Option<f64> {
    let normalized = src.replace('×', "*").replace('÷', "/").replace(',', "");
    let mut p = Parser { s: normalized.as_bytes(), i: 0 };
    let v = p.expr()?;
    p.ws();
    (p.i == p.s.len() && v.is_finite()).then_some(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        loop {
            if self.eat(b'+') {
                v += self.term()?;
            } else if self.eat(b'-') {
                v -= self.term()?;
            } else {
                return Some(v);
            }
        }
    }

    fn term(&mut self) -> Option<f64> {
        let mut v = self.unary()?;
        loop {
            if self.eat(b'*') {
                v *= self.unary()?;
            } else if self.eat(b'/') {
                v /= self.unary()?;
            } else if self.eat(b'%') {
                v %= self.unary()?;
            } else if matches!(self.peek(), Some(b'(') | Some(b'a'..=b'z') | Some(b'A'..=b'Z')) {
                // Implicit multiplication: 2pi, 3(4+1), 2sqrt(9).
                v *= self.unary()?;
            } else {
                return Some(v);
            }
        }
    }

    fn unary(&mut self) -> Option<f64> {
        if self.eat(b'-') {
            return Some(-self.unary()?);
        }
        if self.eat(b'+') {
            return self.unary();
        }
        self.power()
    }

    fn power(&mut self) -> Option<f64> {
        let base = self.atom()?;
        if self.eat(b'^') {
            return Some(base.powf(self.unary()?));
        }
        Some(base)
    }

    fn atom(&mut self) -> Option<f64> {
        if self.eat(b'(') {
            let v = self.expr()?;
            self.eat(b')'); // tolerate a missing closing paren while typing
            return Some(v);
        }
        self.ws();
        let start = self.i;
        if self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            while self.i < self.s.len() && self.s[self.i].is_ascii_alphanumeric() {
                self.i += 1;
            }
            let name = std::str::from_utf8(&self.s[start..self.i]).ok()?.to_ascii_lowercase();
            return match name.as_str() {
                "pi" => Some(std::f64::consts::PI),
                "e" => Some(std::f64::consts::E),
                f => {
                    let arg = self.atom()?;
                    Some(match f {
                        "sqrt" => arg.sqrt(),
                        "sin" => arg.sin(),
                        "cos" => arg.cos(),
                        "tan" => arg.tan(),
                        "asin" => arg.asin(),
                        "acos" => arg.acos(),
                        "atan" => arg.atan(),
                        "ln" => arg.ln(),
                        "log" => arg.log10(),
                        "abs" => arg.abs(),
                        "round" => arg.round(),
                        "floor" => arg.floor(),
                        "ceil" => arg.ceil(),
                        "exp" => arg.exp(),
                        _ => return None,
                    })
                }
            };
        }
        while self.i < self.s.len() && (self.s[self.i].is_ascii_digit() || self.s[self.i] == b'.') {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calc() {
        assert_eq!(eval("2+3*4"), Some(14.0));
        assert_eq!(eval("2^10"), Some(1024.0));
        assert_eq!(eval("sqrt(16)"), Some(4.0));
        assert_eq!(eval("-2^2"), Some(-4.0));
        assert_eq!(format_number(eval("2pi").unwrap()), "6.2831853072");
        assert_eq!(format_number(0.1 + 0.2), "0.3");
        assert_eq!(eval("1,000*2"), Some(2000.0));
        assert_eq!(eval("hello"), None);
    }
}
