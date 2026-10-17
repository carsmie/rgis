//! A small subset of the QGIS expression language (QgsExpression), enough for
//! rule-based renderers, WMS FILTER and WFS EXP_FILTER:
//! `"field" = 'x' AND "pop" >= 1e6 OR "name" IN ('a','b') OR "x" IS NULL OR "n" LIKE 'No%'`
//! plus arithmetic, `||` and a handful of functions.

use anyhow::{Result, anyhow, bail};
use indexmap::IndexMap;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Field(String),
    Op(&'static str),
    LParen,
    RParen,
    Comma,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Lit(Value),
    Field(String),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    In(Box<Expr>, Vec<Expr>, bool),
    IsNull(Box<Expr>, bool),
    Call(String, Vec<Expr>),
}

fn lex(s: &str) -> Result<Vec<Tok>> {
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = vec![];
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '\'' || ch == '"' {
            let q = ch;
            let mut v = String::new();
            i += 1;
            loop {
                if i >= c.len() {
                    bail!("unterminated quote in expression");
                }
                if c[i] == q {
                    if i + 1 < c.len() && c[i + 1] == q {
                        v.push(q);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                if c[i] == '\\' && i + 1 < c.len() {
                    i += 1;
                }
                v.push(c[i]);
                i += 1;
            }
            out.push(if q == '\'' {
                Tok::Str(v)
            } else {
                Tok::Field(v)
            });
        } else if ch.is_ascii_digit()
            || (ch == '.' && c.get(i + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            let st = i;
            while i < c.len()
                && (c[i].is_ascii_alphanumeric()
                    || c[i] == '.'
                    || ((c[i] == '-' || c[i] == '+') && matches!(c[i - 1], 'e' | 'E')))
            {
                i += 1;
            }
            let t: String = c[st..i].iter().collect();
            out.push(Tok::Num(
                t.parse().map_err(|_| anyhow!("bad number '{t}'"))?,
            ));
        } else if ch.is_alphabetic() || ch == '_' || ch == '$' || ch == '@' {
            let st = i;
            while i < c.len()
                && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$' || c[i] == '@')
            {
                i += 1;
            }
            out.push(Tok::Ident(c[st..i].iter().collect()));
        } else {
            let two: String = c[i..(i + 2).min(c.len())].iter().collect();
            let op = match two.as_str() {
                "<>" => Some("<>"),
                "!=" => Some("<>"),
                "<=" => Some("<="),
                ">=" => Some(">="),
                "||" => Some("||"),
                _ => None,
            };
            if let Some(op) = op {
                out.push(Tok::Op(op));
                i += 2;
                continue;
            }
            out.push(match ch {
                '(' => Tok::LParen,
                ')' => Tok::RParen,
                ',' => Tok::Comma,
                '=' => Tok::Op("="),
                '<' => Tok::Op("<"),
                '>' => Tok::Op(">"),
                '+' => Tok::Op("+"),
                '-' => Tok::Op("-"),
                '*' => Tok::Op("*"),
                '/' => Tok::Op("/"),
                '%' => Tok::Op("%"),
                _ => bail!("unexpected character '{ch}' in expression"),
            });
            i += 1;
        }
    }
    Ok(out)
}

struct Parser {
    t: Vec<Tok>,
    p: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.p)
    }
    fn kw(&self, k: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(k))
    }
    fn kw2(&self, a: &str, b: &str) -> bool {
        self.kw(a)
            && matches!(self.t.get(self.p + 1), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(b))
    }
    fn op(&self, o: &str) -> bool {
        matches!(self.peek(), Some(Tok::Op(x)) if *x == o)
    }
    fn expect(&mut self, t: Tok) -> Result<()> {
        if self.peek() == Some(&t) {
            self.p += 1;
            Ok(())
        } else {
            bail!("expected {t:?} in expression")
        }
    }
    fn or(&mut self) -> Result<Expr> {
        let mut l = self.and()?;
        while self.kw("OR") {
            self.p += 1;
            l = Expr::Binary("OR", Box::new(l), Box::new(self.and()?));
        }
        Ok(l)
    }
    fn and(&mut self) -> Result<Expr> {
        let mut l = self.not()?;
        while self.kw("AND") {
            self.p += 1;
            l = Expr::Binary("AND", Box::new(l), Box::new(self.not()?));
        }
        Ok(l)
    }
    fn not(&mut self) -> Result<Expr> {
        if self.kw("NOT") {
            self.p += 1;
            return Ok(Expr::Unary("NOT", Box::new(self.not()?)));
        }
        self.cmp()
    }
    fn list(&mut self) -> Result<Vec<Expr>> {
        self.expect(Tok::LParen)?;
        let mut v = vec![];
        if self.peek() != Some(&Tok::RParen) {
            loop {
                v.push(self.or()?);
                if self.peek() == Some(&Tok::Comma) {
                    self.p += 1;
                } else {
                    break;
                }
            }
        }
        self.expect(Tok::RParen)?;
        Ok(v)
    }
    fn cmp(&mut self) -> Result<Expr> {
        let l = self.concat()?;
        for o in ["=", "<>", "<=", ">=", "<", ">"] {
            if self.op(o) {
                self.p += 1;
                return Ok(Expr::Binary(o, Box::new(l), Box::new(self.concat()?)));
            }
        }
        for k in ["LIKE", "ILIKE"] {
            if self.kw(k) {
                self.p += 1;
                let name = if k == "LIKE" { "LIKE" } else { "ILIKE" };
                return Ok(Expr::Binary(name, Box::new(l), Box::new(self.concat()?)));
            }
        }
        if self.kw("IN") {
            self.p += 1;
            return Ok(Expr::In(Box::new(l), self.list()?, false));
        }
        if self.kw2("NOT", "IN") {
            self.p += 2;
            return Ok(Expr::In(Box::new(l), self.list()?, true));
        }
        if self.kw("IS") {
            self.p += 1;
            let neg = if self.kw("NOT") {
                self.p += 1;
                true
            } else {
                false
            };
            if self.kw("NULL") {
                self.p += 1;
                return Ok(Expr::IsNull(Box::new(l), neg));
            }
            let r = self.concat()?;
            let e = Expr::Binary("=", Box::new(l), Box::new(r));
            return Ok(if neg {
                Expr::Unary("NOT", Box::new(e))
            } else {
                e
            });
        }
        Ok(l)
    }
    fn concat(&mut self) -> Result<Expr> {
        let mut l = self.add()?;
        while self.op("||") {
            self.p += 1;
            l = Expr::Binary("||", Box::new(l), Box::new(self.add()?));
        }
        Ok(l)
    }
    fn add(&mut self) -> Result<Expr> {
        let mut l = self.mul()?;
        loop {
            let o = if self.op("+") {
                "+"
            } else if self.op("-") {
                "-"
            } else {
                break;
            };
            self.p += 1;
            l = Expr::Binary(o, Box::new(l), Box::new(self.mul()?));
        }
        Ok(l)
    }
    fn mul(&mut self) -> Result<Expr> {
        let mut l = self.unary()?;
        loop {
            let o = if self.op("*") {
                "*"
            } else if self.op("/") {
                "/"
            } else if self.op("%") {
                "%"
            } else {
                break;
            };
            self.p += 1;
            l = Expr::Binary(o, Box::new(l), Box::new(self.unary()?));
        }
        Ok(l)
    }
    fn unary(&mut self) -> Result<Expr> {
        if self.op("-") {
            self.p += 1;
            return Ok(Expr::Unary("-", Box::new(self.unary()?)));
        }
        self.primary()
    }
    fn primary(&mut self) -> Result<Expr> {
        let t = self
            .peek()
            .cloned()
            .ok_or_else(|| anyhow!("unexpected end of expression"))?;
        self.p += 1;
        Ok(match t {
            Tok::Num(n) => Expr::Lit(serde_json::json!(n)),
            Tok::Str(s) => Expr::Lit(Value::String(s)),
            Tok::Field(f) => Expr::Field(f),
            Tok::LParen => {
                let e = self.or()?;
                self.expect(Tok::RParen)?;
                e
            }
            Tok::Ident(id) => match id.to_ascii_uppercase().as_str() {
                "TRUE" => Expr::Lit(Value::Bool(true)),
                "FALSE" => Expr::Lit(Value::Bool(false)),
                "NULL" => Expr::Lit(Value::Null),
                _ if self.peek() == Some(&Tok::LParen) => {
                    Expr::Call(id.to_ascii_lowercase(), self.list()?)
                }
                _ => Expr::Field(id),
            },
            other => bail!("unexpected token {other:?} in expression"),
        })
    }
}

pub type Attrs = IndexMap<String, Value>;

impl Expr {
    pub fn parse(s: &str) -> Result<Expr> {
        let mut p = Parser { t: lex(s)?, p: 0 };
        let e = p.or()?;
        if p.p != p.t.len() {
            bail!("unexpected trailing tokens in expression '{s}'");
        }
        Ok(e)
    }

    pub fn matches(&self, attrs: &Attrs) -> bool {
        truthy(&self.eval(attrs))
    }

    pub fn eval(&self, a: &Attrs) -> Value {
        match self {
            Expr::Lit(v) => v.clone(),
            Expr::Field(f) => a
                .get(f)
                .or_else(|| {
                    a.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(f))
                        .map(|(_, v)| v)
                })
                .cloned()
                .unwrap_or(Value::Null),
            Expr::Unary("NOT", e) => match e.eval(a) {
                Value::Null => Value::Null,
                v => Value::Bool(!truthy(&v)),
            },
            Expr::Unary(_, e) => num(&e.eval(a))
                .map(|n| serde_json::json!(-n))
                .unwrap_or(Value::Null),
            Expr::IsNull(e, neg) => Value::Bool(e.eval(a).is_null() != *neg),
            Expr::In(e, list, neg) => {
                let v = e.eval(a);
                if v.is_null() {
                    return Value::Null;
                }
                let found = list
                    .iter()
                    .any(|x| compare(&v, &x.eval(a)) == Some(std::cmp::Ordering::Equal));
                Value::Bool(found != *neg)
            }
            Expr::Binary(op, l, r) => {
                let (lv, rv) = (l.eval(a), r.eval(a));
                match *op {
                    "AND" => match (truthy_opt(&lv), truthy_opt(&rv)) {
                        (Some(false), _) | (_, Some(false)) => Value::Bool(false),
                        (Some(true), Some(true)) => Value::Bool(true),
                        _ => Value::Null,
                    },
                    "OR" => match (truthy_opt(&lv), truthy_opt(&rv)) {
                        (Some(true), _) | (_, Some(true)) => Value::Bool(true),
                        (Some(false), Some(false)) => Value::Bool(false),
                        _ => Value::Null,
                    },
                    "||" => {
                        if lv.is_null() || rv.is_null() {
                            Value::Null
                        } else {
                            Value::String(to_str(&lv) + &to_str(&rv))
                        }
                    }
                    "LIKE" | "ILIKE" => {
                        if lv.is_null() || rv.is_null() {
                            return Value::Null;
                        }
                        Value::Bool(like(&to_str(&lv), &to_str(&rv), *op == "ILIKE"))
                    }
                    "+" | "-" | "*" | "/" | "%" => match (num(&lv), num(&rv)) {
                        (Some(x), Some(y)) => {
                            let v = match *op {
                                "+" => x + y,
                                "-" => x - y,
                                "*" => x * y,
                                "/" if y != 0.0 => x / y,
                                "%" if y != 0.0 => x % y,
                                _ => return Value::Null,
                            };
                            serde_json::json!(v)
                        }
                        _ if *op == "+" && lv.is_string() && rv.is_string() => {
                            Value::String(to_str(&lv) + &to_str(&rv))
                        }
                        _ => Value::Null,
                    },
                    cmp_op => {
                        if lv.is_null() || rv.is_null() {
                            return Value::Null;
                        }
                        let Some(o) = compare(&lv, &rv) else {
                            return Value::Null;
                        };
                        use std::cmp::Ordering::*;
                        Value::Bool(match cmp_op {
                            "=" => o == Equal,
                            "<>" => o != Equal,
                            "<" => o == Less,
                            ">" => o == Greater,
                            "<=" => o != Greater,
                            ">=" => o != Less,
                            _ => false,
                        })
                    }
                }
            }
            Expr::Call(f, args) => {
                let v: Vec<Value> = args.iter().map(|e| e.eval(a)).collect();
                let first = v.first().cloned().unwrap_or(Value::Null);
                match f.as_str() {
                    "lower" => first
                        .as_str()
                        .map(|s| Value::String(s.to_lowercase()))
                        .unwrap_or(Value::Null),
                    "upper" => first
                        .as_str()
                        .map(|s| Value::String(s.to_uppercase()))
                        .unwrap_or(Value::Null),
                    "length" => first
                        .as_str()
                        .map(|s| serde_json::json!(s.chars().count()))
                        .unwrap_or(Value::Null),
                    "coalesce" => v.into_iter().find(|x| !x.is_null()).unwrap_or(Value::Null),
                    "to_string" | "tostring" => {
                        if first.is_null() {
                            Value::Null
                        } else {
                            Value::String(to_str(&first))
                        }
                    }
                    "to_int" | "toint" => num(&first)
                        .map(|n| serde_json::json!(n.trunc() as i64))
                        .unwrap_or(Value::Null),
                    "to_real" | "toreal" => num(&first)
                        .map(|n| serde_json::json!(n))
                        .unwrap_or(Value::Null),
                    "abs" => num(&first)
                        .map(|n| serde_json::json!(n.abs()))
                        .unwrap_or(Value::Null),
                    "round" => {
                        let places = v.get(1).and_then(num).unwrap_or(0.0) as i32;
                        num(&first)
                            .map(|n| {
                                serde_json::json!(
                                    (n * 10f64.powi(places)).round() / 10f64.powi(places)
                                )
                            })
                            .unwrap_or(Value::Null)
                    }
                    "concat" => {
                        Value::String(v.iter().filter(|x| !x.is_null()).map(to_str).collect())
                    }
                    "attribute" => first
                        .as_str()
                        .and_then(|n| a.get(n).cloned())
                        .unwrap_or(Value::Null),
                    _ => Value::Null,
                }
            }
        }
    }
}

fn truthy_opt(v: &Value) -> Option<bool> {
    if v.is_null() { None } else { Some(truthy(v)) }
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty() && num(v).is_none_or(|f| f != 0.0),
        _ => true,
    }
}

pub fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

pub fn to_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => i.to_string(),
            (_, Some(f)) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

/// Numeric comparison when both sides are numeric, string comparison otherwise.
pub fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    let numeric = |v: &Value| matches!(v, Value::Number(_) | Value::Bool(_));
    if (numeric(a) || numeric(b))
        && let (Some(x), Some(y)) = (num(a), num(b)) {
            return x.partial_cmp(&y);
        }
    Some(to_str(a).cmp(&to_str(b)))
}

fn like(s: &str, pat: &str, ci: bool) -> bool {
    let (s, pat): (Vec<char>, Vec<char>) = if ci {
        (
            s.to_lowercase().chars().collect(),
            pat.to_lowercase().chars().collect(),
        )
    } else {
        (s.chars().collect(), pat.chars().collect())
    };
    fn m(s: &[char], p: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('%') => (0..=s.len()).any(|i| m(&s[i..], &p[1..])),
            Some('_') => !s.is_empty() && m(&s[1..], &p[1..]),
            Some(c) => s.first() == Some(c) && m(&s[1..], &p[1..]),
        }
    }
    m(&s, &pat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attrs() -> Attrs {
        let mut a = Attrs::new();
        a.insert("NAME".into(), json!("Norway"));
        a.insert("pop".into(), json!(5_400_000));
        a.insert("note".into(), Value::Null);
        a
    }

    #[test]
    fn evaluates_common_filters() {
        let a = attrs();
        for (e, want) in [
            (r#""NAME" = 'Norway'"#, true),
            (r#""NAME" = 'Sweden'"#, false),
            (r#""pop" > 5e6 AND "NAME" LIKE 'Nor%'"#, true),
            (r#""NAME" IN ('Sweden', 'Norway')"#, true),
            (r#""NAME" NOT IN ('Norway')"#, false),
            (r#""note" IS NULL"#, true),
            (r#""note" IS NOT NULL OR "pop" / 2 = 2700000"#, true),
            (r#"lower("NAME") = 'norway'"#, true),
            (r#"NOT ("pop" < 100)"#, true),
            (r#""NAME" || '!' = 'Norway!'"#, true),
            ("name ILIKE 'NOR_AY'", true),
        ] {
            assert_eq!(Expr::parse(e).unwrap().matches(&a), want, "{e}");
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(Expr::parse("\"a\" = ").is_err());
        assert!(Expr::parse("'unterminated").is_err());
    }
}
