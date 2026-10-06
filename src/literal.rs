//! A bounded, non-executing reader for the Python literals in the v1 config.
use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Tuple(Vec<Value>),
    List(Vec<Value>),
    Dict(Vec<(Value, Value)>),
}
impl Value {
    pub fn number(&self) -> Option<f64> {
        match self {
            Self::Int(n) => Some(*n as f64),
            Self::Float(n) if n.is_finite() => Some(*n),
            _ => None,
        }
    }
    pub fn string(&self) -> Option<&str> {
        if let Self::String(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub fn dict(&self) -> Option<&[(Value, Value)]> {
        if let Self::Dict(d) = self {
            Some(d)
        } else {
            None
        }
    }
    pub fn tuple(&self, n: usize) -> Option<&[Value]> {
        match self {
            Self::Tuple(v) if v.len() == n => Some(v),
            _ => None,
        }
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.dict()?
            .iter()
            .rev()
            .find(|(k, _)| k.string() == Some(key))
            .map(|(_, v)| v)
    }
    pub fn get_int(&self, key: i64) -> Option<&Value> {
        self.dict()?
            .iter()
            .rev()
            .find(|(k, _)| *k == Self::Int(key))
            .map(|(_, v)| v)
    }
    pub fn object(items: impl IntoIterator<Item = (String, Value)>) -> Self {
        Self::Dict(
            items
                .into_iter()
                .map(|(k, v)| (Self::String(k), v))
                .collect(),
        )
    }
    pub fn array(items: impl IntoIterator<Item = Value>) -> Self {
        Self::List(items.into_iter().collect())
    }
    pub fn merge(&mut self, new: Value) {
        match (self, new) {
            (Self::Dict(old), Self::Dict(new)) => {
                for (k, v) in new {
                    if let Some((_, ov)) = old.iter_mut().find(|(ok, _)| *ok == k) {
                        // v1 merges one nested dictionary level, not arbitrary recursion.
                        match (ov, v) {
                            (Self::Dict(od), Self::Dict(nd)) => {
                                for (nk, nv) in nd {
                                    if let Some((_, ov)) = od.iter_mut().find(|(ok, _)| *ok == nk) {
                                        *ov = nv;
                                    } else {
                                        od.push((nk, nv));
                                    }
                                }
                            }
                            (ov, v) => *ov = v,
                        }
                    } else {
                        old.push((k, v));
                    }
                }
            }
            (old, new) => *old = new,
        }
    }
    pub fn json(&self) -> String {
        fn quoted(s: &str, out: &mut String) {
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c < ' ' => {
                        let _ = write!(out, "\\u{:04x}", c as u32);
                    }
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        fn emit(v: &Value, out: &mut String) {
            match v {
                Value::None => out.push_str("null"),
                Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                Value::Int(n) => {
                    let _ = write!(out, "{n}");
                }
                Value::Float(n) if n.is_finite() => {
                    let _ = write!(out, "{n}");
                }
                Value::Float(_) => out.push_str("null"),
                Value::String(s) => quoted(s, out),
                Value::Tuple(v) | Value::List(v) => {
                    out.push('[');
                    for (i, v) in v.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        emit(v, out);
                    }
                    out.push(']');
                }
                Value::Dict(d) => {
                    out.push('{');
                    for (i, (k, v)) in d.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        match k {
                            Value::String(s) => quoted(s, out),
                            Value::Int(n) => quoted(&n.to_string(), out),
                            _ => quoted(&k.json(), out),
                        }
                        out.push_str(": ");
                        emit(v, out);
                    }
                    out.push('}');
                }
            }
        }
        let mut s = String::new();
        emit(self, &mut s);
        s
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Self::String(s.into())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}
impl From<f64> for Value {
    fn from(n: f64) -> Self {
        if n.is_finite() {
            Self::Float(n)
        } else {
            Self::None
        }
    }
}
impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Self::Int(n)
    }
}
impl From<usize> for Value {
    fn from(n: usize) -> Self {
        Self::Int(n as i64)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}
impl From<Option<f64>> for Value {
    fn from(n: Option<f64>) -> Self {
        n.map(Self::from).unwrap_or(Self::None)
    }
}

pub fn parse(input: &str) -> Result<Value, String> {
    if input.len() > 131_072 {
        return Err("literal exceeds 128 KiB".into());
    }
    let mut p = Parser {
        s: input,
        pos: 0,
        nodes: 0,
    };
    let v = p.value(0)?;
    p.space();
    if p.pos != p.s.len() {
        return Err(p.error("unexpected trailing text"));
    }
    Ok(v)
}
struct Parser<'a> {
    s: &'a str,
    pos: usize,
    nodes: usize,
}
impl Parser<'_> {
    fn error(&self, msg: &str) -> String {
        format!("{msg} at byte {}", self.pos)
    }
    fn peek(&self) -> Option<char> {
        self.s[self.pos..].chars().next()
    }
    fn take(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }
    fn space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.take();
        }
    }
    fn eat(&mut self, c: char) -> bool {
        self.space();
        if self.peek() == Some(c) {
            self.take();
            true
        } else {
            false
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value, String> {
        self.space();
        self.nodes += 1;
        if depth > 32 || self.nodes > 16_384 {
            return Err(self.error("literal nesting/size limit"));
        }
        match self.peek() {
            Some('\'' | '"') => self.string(),
            Some('u' | 'U') if self.s[self.pos + 1..].starts_with(['\'', '"']) => {
                self.take();
                self.string()
            }
            Some('(' | '[') => {
                let open = self.take().unwrap();
                let end = if open == '(' { ')' } else { ']' };
                let mut values = Vec::new();
                let mut comma = false;
                if !self.eat(end) {
                    loop {
                        values.push(self.value(depth + 1)?);
                        if self.eat(end) {
                            break;
                        }
                        if !self.eat(',') {
                            return Err(self.error("expected comma"));
                        }
                        comma = true;
                        if self.eat(end) {
                            break;
                        }
                    }
                }
                if open == '(' && values.len() == 1 && !comma {
                    Ok(values.remove(0))
                } else if open == '(' {
                    Ok(Value::Tuple(values))
                } else {
                    Ok(Value::List(values))
                }
            }
            Some('{') => {
                self.take();
                let mut entries = Vec::new();
                if !self.eat('}') {
                    loop {
                        let key = self.value(depth + 1)?;
                        if !matches!(
                            key,
                            Value::String(_)
                                | Value::Int(_)
                                | Value::Float(_)
                                | Value::Bool(_)
                                | Value::None
                        ) {
                            return Err(self.error("unsupported dictionary key"));
                        }
                        if !self.eat(':') {
                            return Err(self.error("expected colon"));
                        }
                        let val = self.value(depth + 1)?;
                        if let Some((_, ov)) = entries.iter_mut().find(|(k, _)| *k == key) {
                            *ov = val;
                        } else {
                            entries.push((key, val));
                        }
                        if self.eat('}') {
                            break;
                        }
                        if !self.eat(',') {
                            return Err(self.error("expected comma"));
                        }
                        if self.eat('}') {
                            break;
                        }
                    }
                }
                Ok(Value::Dict(entries))
            }
            Some(c) if c.is_ascii_digit() || matches!(c, '+' | '-' | '.') => self.number(),
            _ => {
                for (word, v) in [
                    ("None", Value::None),
                    ("True", Value::Bool(true)),
                    ("False", Value::Bool(false)),
                ] {
                    if self.s[self.pos..].starts_with(word) {
                        self.pos += word.len();
                        return Ok(v);
                    }
                }
                Err(self.error("expected a literal (expressions are not allowed)"))
            }
        }
    }
    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '_'))
        {
            self.take();
        }
        let raw = self.s[start..self.pos].replace('_', "");
        if let Ok(n) = raw.parse::<i64>() {
            return Ok(Value::Int(n));
        }
        let (negative, unsigned) = if let Some(s) = raw.strip_prefix('-') {
            (true, s)
        } else {
            (false, raw.strip_prefix('+').unwrap_or(&raw))
        };
        for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
            if let Some(digits) = unsigned.to_ascii_lowercase().strip_prefix(prefix) {
                let n = i64::from_str_radix(digits, radix)
                    .map_err(|_| self.error("invalid integer"))?;
                return Ok(Value::Int(if negative { -n } else { n }));
            }
        }
        let n: f64 = raw.parse().map_err(|_| self.error("invalid number"))?;
        if !n.is_finite() {
            return Err(self.error("number must be finite"));
        }
        Ok(Value::Float(n))
    }
    fn string(&mut self) -> Result<Value, String> {
        let quote = self.take().unwrap();
        let mut out = String::new();
        loop {
            match self
                .take()
                .ok_or_else(|| self.error("unterminated string"))?
            {
                c if c == quote => return Ok(Value::String(out)),
                '\n' | '\r' => return Err(self.error("newline in string")),
                '\\' => {
                    let c = self
                        .take()
                        .ok_or_else(|| self.error("unterminated escape"))?;
                    match c {
                        '\\' | '\'' | '"' => out.push(c),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'a' => out.push('\u{7}'),
                        'v' => out.push('\u{b}'),
                        'x' | 'u' | 'U' => {
                            let mut n = 0u32;
                            for _ in 0..match c {
                                'x' => 2,
                                'u' => 4,
                                _ => 8,
                            } {
                                n = n
                                    .checked_mul(16)
                                    .and_then(|n| {
                                        self.take()?.to_digit(16).and_then(|d| n.checked_add(d))
                                    })
                                    .ok_or_else(|| self.error("invalid character escape"))?;
                            }
                            out.push(
                                char::from_u32(n)
                                    .ok_or_else(|| self.error("invalid Unicode scalar"))?,
                            );
                        }
                        c if matches!(c, '0'..='7') => {
                            let mut n = c.to_digit(8).unwrap();
                            for _ in 0..2 {
                                if let Some(d) = self.peek().and_then(|p| p.to_digit(8)) {
                                    self.take();
                                    n = n * 8 + d;
                                } else {
                                    break;
                                }
                            }
                            out.push(char::from_u32(n).unwrap());
                        }
                        c => {
                            out.push('\\');
                            out.push(c);
                        }
                    }
                }
                c => out.push(c),
            }
            if out.len() > 8192 {
                return Err(self.error("string exceeds 8 KiB"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn python_literals() {
        assert_eq!(
            parse("{'max': 125, 1: (True, None, 1.5,)}")
                .unwrap()
                .get("max"),
            Some(&Value::Int(125))
        );
        assert_eq!(parse("(1)").unwrap(), Value::Int(1));
        assert!(matches!(parse("(1,)").unwrap(), Value::Tuple(_)));
        assert_eq!(parse("'a\\u00b0\\n'").unwrap(), Value::from("a°\n"));
        assert_eq!(parse("0x7d").unwrap(), Value::Int(125));
    }
    #[test]
    fn never_evaluates_and_bounded() {
        for s in [
            "__import__('os').system('id')",
            "nan",
            "1e999",
            "{**{}}",
            "[1]*100000",
        ] {
            assert!(parse(s).is_err(), "{s}");
        }
        assert!(parse(&format!("{}0{}", "[".repeat(40), "]".repeat(40))).is_err());
        assert!(parse(&" ".repeat(131_073)).is_err());
        assert_eq!(Value::from("\"\n").json(), "\"\\\"\\n\"");
    }
}
