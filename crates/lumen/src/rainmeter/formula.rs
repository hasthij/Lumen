//! Rainmeter formula evaluation: numbers, + - * / % ^, unary minus, parentheses.

pub fn eval(src: &str) -> Option<f64> {
    let mut p = Parser { s: src.as_bytes(), i: 0 };
    let v = p.expr()?;
    p.ws();
    (p.i == p.s.len()).then_some(v).filter(|v| v.is_finite())
}

/// Reads a numeric option: a plain number, or a formula when wrapped in parentheses.
pub fn number(src: &str) -> Option<f64> {
    let s = src.trim();
    s.parse::<f64>().ok().or_else(|| eval(s))
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

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.s.get(self.i) == Some(&c) {
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
        let mut v = self.power()?;
        loop {
            if self.eat(b'*') {
                v *= self.power()?;
            } else if self.eat(b'/') {
                let d = self.power()?;
                v = if d == 0.0 { 0.0 } else { v / d };
            } else if self.eat(b'%') {
                let d = self.power()?;
                v = if d == 0.0 { 0.0 } else { v % d };
            } else {
                return Some(v);
            }
        }
    }

    fn power(&mut self) -> Option<f64> {
        let base = self.unary()?;
        if self.eat(b'^') {
            Some(base.powf(self.power()?))
        } else {
            Some(base)
        }
    }

    fn unary(&mut self) -> Option<f64> {
        if self.eat(b'-') {
            return Some(-self.unary()?);
        }
        if self.eat(b'+') {
            return self.unary();
        }
        self.atom()
    }

    fn atom(&mut self) -> Option<f64> {
        if self.eat(b'(') {
            let v = self.expr()?;
            return self.eat(b')').then_some(v);
        }
        self.ws();
        let start = self.i;
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
    fn formulas() {
        assert_eq!(eval("(100*1.3)"), Some(130.0));
        assert_eq!(eval("(1 + 2) * -3"), Some(-9.0));
        assert_eq!(eval("2^3^2"), Some(512.0));
        assert_eq!(number("12"), Some(12.0));
        assert_eq!(number("(10*1.3)").map(|v| (v * 10.0).round()), Some(130.0));
        assert_eq!(eval("(1+"), None);
    }
}
