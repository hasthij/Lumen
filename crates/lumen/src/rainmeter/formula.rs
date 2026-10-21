//! Rainmeter formula evaluation: numbers, + - * / % ^, unary minus, parentheses, functions
//! (Max, Min, Abs, Round, ...) and section references like `[MeterName:W]`.

type Lookup<'a> = &'a dyn Fn(&str) -> Option<f64>;

pub fn eval(src: &str) -> Option<f64> {
    eval_with(src, &|_| None)
}

/// Reads a numeric option: a plain number, or a formula when wrapped in parentheses.
pub fn number(src: &str) -> Option<f64> {
    number_with(src, &|_| None)
}

/// Like `number`, but `[Name]` and `[Name:Property]` references are answered by `lookup`
/// (unknown references count as 0, as in Rainmeter before a value exists).
pub fn number_with(src: &str, lookup: Lookup) -> Option<f64> {
    let s = src.trim();
    s.parse::<f64>().ok().or_else(|| eval_with(s, lookup))
}

pub fn eval_with(src: &str, lookup: Lookup) -> Option<f64> {
    let mut p = Parser { s: src.as_bytes(), i: 0, lookup };
    let v = p.expr()?;
    p.ws();
    (p.i == p.s.len()).then_some(v).filter(|v| v.is_finite())
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    lookup: Lookup<'a>,
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
        // [Section:Property] or [Measure]
        if self.s.get(self.i) == Some(&b'[') {
            let start = self.i + 1;
            let end = start + self.s[start..].iter().position(|&c| c == b']')?;
            let name = std::str::from_utf8(&self.s[start..end]).ok()?.trim().to_string();
            self.i = end + 1;
            return Some((self.lookup)(&name).unwrap_or(0.0));
        }
        // function call
        if self.s.get(self.i).is_some_and(|c| c.is_ascii_alphabetic()) {
            let start = self.i;
            while self.i < self.s.len() && self.s[self.i].is_ascii_alphanumeric() {
                self.i += 1;
            }
            let name = std::str::from_utf8(&self.s[start..self.i]).ok()?.to_ascii_lowercase();
            if !self.eat(b'(') {
                return None;
            }
            let mut args = Vec::new();
            if !self.eat(b')') {
                loop {
                    args.push(self.expr()?);
                    if self.eat(b',') {
                        continue;
                    }
                    if self.eat(b')') {
                        break;
                    }
                    return None;
                }
            }
            return call(&name, &args);
        }
        let start = self.i;
        while self.i < self.s.len() && (self.s[self.i].is_ascii_digit() || self.s[self.i] == b'.') {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()
    }
}

fn call(name: &str, a: &[f64]) -> Option<f64> {
    let one = || a.first().copied();
    Some(match name {
        "max" if !a.is_empty() => a.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        "min" if !a.is_empty() => a.iter().copied().fold(f64::INFINITY, f64::min),
        "clamp" if a.len() == 3 => a[0].clamp(a[1].min(a[2]), a[1].max(a[2])),
        "abs" => one()?.abs(),
        "ceil" => one()?.ceil(),
        "floor" => one()?.floor(),
        "round" => one()?.round(),
        "trunc" => one()?.trunc(),
        "frac" => one()?.fract(),
        "sign" => {
            let v = one()?;
            if v > 0.0 {
                1.0
            } else if v < 0.0 {
                -1.0
            } else {
                0.0
            }
        }
        "sqrt" => one()?.sqrt(),
        "sin" => one()?.sin(),
        "cos" => one()?.cos(),
        "tan" => one()?.tan(),
        "asin" => one()?.asin(),
        "acos" => one()?.acos(),
        "atan" => one()?.atan(),
        "ln" => one()?.ln(),
        "log" => one()?.log10(),
        "exp" => one()?.exp(),
        _ => return None,
    })
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

    #[test]
    fn functions_and_references() {
        let look = |n: &str| match n {
            "MeterHours:W" => Some(200.0),
            "MeterMinutes:W" => Some(150.0),
            "MeterDate:Y" => Some(300.0),
            _ => None,
        };
        let v = eval_with("((Max([MeterHours:W],[MeterMinutes:W])*1.2)+(20*1.5))", &look);
        assert_eq!(v, Some(270.0));
        assert_eq!(eval_with("([MeterDate:Y]+[Missing:H]+(10*1.5))", &look), Some(315.0));
        assert_eq!(eval("Min(4, 2, 9)"), Some(2.0));
        assert_eq!(eval("Clamp(15, 0, 10)"), Some(10.0));
        assert_eq!(eval("Round(2.5)"), Some(3.0));
    }
}
