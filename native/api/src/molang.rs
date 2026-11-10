//! The small part of Molang that Cobblemon-style animations use in keyframe values:
//! numbers, `+ - * /`, brackets, `math.sin/cos` (degrees, as in Bedrock), `math.abs`,
//! `math.clamp`, `math.lerp`, `math.min/max`, `math.sqrt`, `math.floor/ceil/round`,
//! `math.mod`, `math.pi`, and `query.anim_time` / `q.anim_time` (also `life_time`).
//! Names are case-insensitive. Any other variable or query reads as 0.

#[derive(Clone, Debug)]
pub enum Expr {
    Num(f32),
    Time,
    Neg(Box<Expr>),
    Bin(char, Box<Expr>, Box<Expr>),
    Call(Func, Vec<Expr>),
}

#[derive(Clone, Copy, Debug)]
pub enum Func {
    Sin,
    Cos,
    Abs,
    Clamp,
    Lerp,
    Min,
    Max,
    Sqrt,
    Floor,
    Ceil,
    Round,
    Mod,
}

impl Expr {
    pub fn eval(&self, t: f32) -> f32 {
        let v = match self {
            Self::Num(n) => *n,
            Self::Time => t,
            Self::Neg(e) => -e.eval(t),
            Self::Bin(op, a, b) => {
                let (a, b) = (a.eval(t), b.eval(t));
                match op {
                    '+' => a + b,
                    '-' => a - b,
                    '*' => a * b,
                    _ => {
                        if b == 0.0 {
                            0.0
                        } else {
                            a / b
                        }
                    }
                }
            }
            Self::Call(f, args) => {
                let a = |i: usize| args.get(i).map(|e| e.eval(t)).unwrap_or(0.0);
                match f {
                    Func::Sin => a(0).to_radians().sin(),
                    Func::Cos => a(0).to_radians().cos(),
                    Func::Abs => a(0).abs(),
                    Func::Clamp => a(0).max(a(1)).min(a(2)),
                    Func::Lerp => a(0) + (a(1) - a(0)) * a(2),
                    Func::Min => a(0).min(a(1)),
                    Func::Max => a(0).max(a(1)),
                    Func::Sqrt => a(0).max(0.0).sqrt(),
                    Func::Floor => a(0).floor(),
                    Func::Ceil => a(0).ceil(),
                    Func::Round => a(0).round(),
                    Func::Mod => {
                        let b = a(1);
                        if b == 0.0 {
                            0.0
                        } else {
                            a(0) % b
                        }
                    }
                }
            }
        };
        if v.is_finite() { v } else { 0.0 }
    }
}

#[derive(Debug, PartialEq)]
enum Tok {
    Num(f32),
    Name(String),
    Op(char),
}

fn lex(s: &str) -> Result<Vec<Tok>, String> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch.is_ascii_digit() || (ch == '.' && c.get(i + 1).is_some_and(char::is_ascii_digit)) {
            let start = i;
            while i < c.len() && (c[i].is_ascii_digit() || c[i] == '.') {
                i += 1;
            }
            let text: String = c[start..i].iter().collect();
            out.push(Tok::Num(text.parse().map_err(|_| format!("bad number {text}"))?));
            // Optional float suffix ("1.5f").
            if i < c.len() && (c[i] == 'f' || c[i] == 'F') {
                i += 1;
            }
        } else if ch.is_ascii_alphabetic() || ch == '_' {
            let start = i;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '_' || c[i] == '.') {
                i += 1;
            }
            out.push(Tok::Name(c[start..i].iter().collect::<String>().to_ascii_lowercase()));
        } else if "+-*/(),".contains(ch) {
            out.push(Tok::Op(ch));
            i += 1;
        } else if ch == ';' {
            i += 1;
        } else {
            return Err(format!("unsupported '{ch}'"));
        }
    }
    Ok(out)
}

struct P {
    t: Vec<Tok>,
    i: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn op(&mut self, o: char) -> bool {
        if self.peek() == Some(&Tok::Op(o)) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expr(&mut self) -> Result<Expr, String> {
        let mut a = self.term()?;
        loop {
            let o = if self.op('+') {
                '+'
            } else if self.op('-') {
                '-'
            } else {
                return Ok(a);
            };
            a = Expr::Bin(o, Box::new(a), Box::new(self.term()?));
        }
    }
    fn term(&mut self) -> Result<Expr, String> {
        let mut a = self.unary()?;
        loop {
            let o = if self.op('*') {
                '*'
            } else if self.op('/') {
                '/'
            } else {
                return Ok(a);
            };
            a = Expr::Bin(o, Box::new(a), Box::new(self.unary()?));
        }
    }
    fn unary(&mut self) -> Result<Expr, String> {
        if self.op('-') {
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        if self.op('+') {
            return self.unary();
        }
        self.primary()
    }
    fn primary(&mut self) -> Result<Expr, String> {
        if self.op('(') {
            let e = self.expr()?;
            if !self.op(')') {
                return Err("missing )".into());
            }
            return Ok(e);
        }
        match self.t.get(self.i) {
            Some(Tok::Num(n)) => {
                let n = *n;
                self.i += 1;
                Ok(Expr::Num(n))
            }
            Some(Tok::Name(name)) => {
                let name = name.clone();
                self.i += 1;
                if self.op('(') {
                    let mut args = Vec::new();
                    if !self.op(')') {
                        loop {
                            args.push(self.expr()?);
                            if self.op(')') {
                                break;
                            }
                            if !self.op(',') {
                                return Err("expected , or )".into());
                            }
                        }
                    }
                    let f = match name.as_str() {
                        "math.sin" => Func::Sin,
                        "math.cos" => Func::Cos,
                        "math.abs" => Func::Abs,
                        "math.clamp" => Func::Clamp,
                        "math.lerp" => Func::Lerp,
                        "math.min" => Func::Min,
                        "math.max" => Func::Max,
                        "math.sqrt" => Func::Sqrt,
                        "math.floor" => Func::Floor,
                        "math.ceil" => Func::Ceil,
                        "math.round" => Func::Round,
                        "math.mod" => Func::Mod,
                        _ => return Ok(Expr::Num(0.0)),
                    };
                    return Ok(Expr::Call(f, args));
                }
                Ok(match name.as_str() {
                    "q.anim_time" | "query.anim_time" | "q.life_time" | "query.life_time" => Expr::Time,
                    "math.pi" => Expr::Num(std::f32::consts::PI),
                    _ => Expr::Num(0.0),
                })
            }
            other => Err(format!("unexpected {other:?}")),
        }
    }
}

/// Parse one Molang expression. Errors on syntax this subset does not cover.
pub fn parse(s: &str) -> Result<Expr, String> {
    let mut p = P { t: lex(s)?, i: 0 };
    let e = p.expr()?;
    if p.i != p.t.len() {
        return Err(format!("trailing input in {s:?}"));
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(s: &str, t: f32) -> f32 {
        parse(s).unwrap().eval(t)
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(ev("1 + 2 * 3", 0.0), 7.0);
        assert_eq!(ev("(1 + 2) * 3", 0.0), 9.0);
        assert_eq!(ev("0-2*-3", 0.0), 6.0);
        assert_eq!(ev(".5", 0.0), 0.5);
    }

    #[test]
    fn trig_in_degrees_and_time() {
        assert!((ev("math.sin(90)", 0.0) - 1.0).abs() < 1e-6);
        assert!((ev("Math.cos((query.anim_time - 0.2) * 90) * 2", 0.2) - 2.0).abs() < 1e-5);
        assert!((ev("1.8514+math.sin((q.anim_time)*90*2)*2", 0.5) - 3.8514).abs() < 1e-4);
        assert_eq!(ev("math.clamp(q.anim_time * 10, 0, 3)", 1.0), 3.0);
    }

    #[test]
    fn unknown_names_are_zero() {
        assert_eq!(ev("v.something + 2", 0.0), 2.0);
        assert_eq!(ev("query.ground_speed * 3 + 1", 0.0), 1.0);
    }
}
