//! Bounded, strict JSON structure reader for Cloud Hypervisor responses.
//! Strings retain JSON escapes; comparisons use our JSON encoder (no substring matching).
use crate::error::{Error, Result};
use std::collections::BTreeMap;

#[derive(Debug)]
pub(crate) enum Value<'a> {
    Object(BTreeMap<&'a str, Value<'a>>),
    Array(Vec<Value<'a>>),
    String(&'a str),
    Atom(&'a str),
}
impl<'a> Value<'a> {
    pub fn object(&self) -> Result<&BTreeMap<&'a str, Value<'a>>> {
        match self {
            Self::Object(v) => Ok(v),
            _ => Err(invalid()),
        }
    }
    pub fn array(&self) -> Result<&[Value<'a>]> {
        match self {
            Self::Array(v) => Ok(v),
            _ => Err(invalid()),
        }
    }
    pub fn is_string(&self, expected: &str) -> bool {
        let encoded = crate::output::json_string(expected);
        matches!(self, Self::String(raw) if *raw == &encoded[1..encoded.len()-1])
    }
    pub fn is_false(&self) -> bool {
        matches!(self, Self::Atom("false"))
    }
}
fn invalid() -> Error {
    Error::Resource("invalid Cloud Hypervisor API JSON".into())
}
struct Reader<'a> {
    input: &'a str,
    pos: usize,
}
impl<'a> Reader<'a> {
    fn ws(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.pos)
            .is_some_and(|b| [b' ', b'\t', b'\r', b'\n'].contains(b))
        {
            self.pos += 1;
        }
    }
    fn byte(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }
    fn token(&mut self, token: u8) -> Result<()> {
        self.ws();
        if self.byte() != Some(token) {
            return Err(invalid());
        }
        self.pos += 1;
        Ok(())
    }
    fn string(&mut self) -> Result<&'a str> {
        self.token(b'"')?;
        let start = self.pos;
        loop {
            match self.byte() {
                Some(b'"') => {
                    let end = self.pos;
                    self.pos += 1;
                    return Ok(&self.input[start..end]);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.byte() {
                        Some(b'u') => {
                            self.pos += 1;
                            let code = self.hex()?;
                            if (0xd800..=0xdbff).contains(&code) {
                                if self.input.as_bytes().get(self.pos..self.pos + 2) != Some(b"\\u")
                                {
                                    return Err(invalid());
                                }
                                self.pos += 2;
                                if !(0xdc00..=0xdfff).contains(&self.hex()?) {
                                    return Err(invalid());
                                }
                            } else if (0xdc00..=0xdfff).contains(&code) {
                                return Err(invalid());
                            }
                            continue;
                        }
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.pos += 1
                        }
                        _ => return Err(invalid()),
                    }
                }
                Some(b) if b >= 0x20 => self.pos += 1,
                _ => return Err(invalid()),
            }
        }
    }
    fn hex(&mut self) -> Result<u16> {
        let mut code = 0;
        for _ in 0..4 {
            let digit = self
                .byte()
                .and_then(|b| (b as char).to_digit(16))
                .ok_or_else(invalid)?;
            code = code * 16 + digit as u16;
            self.pos += 1;
        }
        Ok(code)
    }
    fn value(&mut self, depth: usize) -> Result<Value<'a>> {
        if depth > 64 {
            return Err(invalid());
        }
        self.ws();
        match self.byte() {
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b'{') => {
                self.pos += 1;
                let mut values = BTreeMap::new();
                self.ws();
                if self.byte() != Some(b'}') {
                    loop {
                        let key = self.string()?;
                        // CH emits unescaped field names and ASCII device IDs. Reject
                        // escaped keys to prevent equivalent duplicate-key ambiguity.
                        if key.contains('\\') {
                            return Err(invalid());
                        }
                        self.token(b':')?;
                        if values.insert(key, self.value(depth + 1)?).is_some() {
                            return Err(invalid());
                        }
                        self.ws();
                        if self.byte() == Some(b'}') {
                            break;
                        }
                        self.token(b',')?;
                    }
                }
                self.token(b'}')?;
                Ok(Value::Object(values))
            }
            Some(b'[') => {
                self.pos += 1;
                let mut values = Vec::new();
                self.ws();
                if self.byte() != Some(b']') {
                    loop {
                        values.push(self.value(depth + 1)?);
                        self.ws();
                        if self.byte() == Some(b']') {
                            break;
                        }
                        self.token(b',')?;
                    }
                }
                self.token(b']')?;
                Ok(Value::Array(values))
            }
            Some(_) => {
                let start = self.pos;
                while self
                    .byte()
                    .is_some_and(|b| ![b' ', b'\t', b'\r', b'\n', b',', b'}', b']'].contains(&b))
                {
                    self.pos += 1;
                }
                let atom = &self.input[start..self.pos];
                if !["true", "false", "null"].contains(&atom) && !number(atom) {
                    return Err(invalid());
                }
                Ok(Value::Atom(atom))
            }
            _ => Err(invalid()),
        }
    }
}
fn number(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let mut pos = usize::from(bytes.first() == Some(&b'-'));
    match bytes.get(pos) {
        Some(b'0') => pos += 1,
        Some(b'1'..=b'9') => {
            while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
                pos += 1;
            }
        }
        _ => return false,
    }
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        let start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        if start == pos {
            return false;
        }
    }
    if bytes.get(pos).is_some_and(|b| [b'e', b'E'].contains(b)) {
        pos += 1;
        if bytes.get(pos).is_some_and(|b| [b'+', b'-'].contains(b)) {
            pos += 1;
        }
        let start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        if start == pos {
            return false;
        }
    }
    pos == bytes.len()
}
pub(crate) fn parse(bytes: &[u8]) -> Result<Value<'_>> {
    if bytes.len() > 1024 * 1024 {
        return Err(invalid());
    }
    let input = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    let mut reader = Reader { input, pos: 0 };
    let value = reader.value(0)?;
    reader.ws();
    if reader.pos != input.len() {
        return Err(invalid());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_duplicates_bad_numbers_escapes_depth_and_trailing_data() {
        for raw in [
            r#"{"id":"a","id":"b"}"#,
            r#"{"\u0069d":"a"}"#,
            "[01]",
            "[NaN]",
            "[1.]",
            "[+1]",
            "[1e]",
            "{}x",
            "[1,]",
            r#""\ud800""#,
            r#""\udc00""#,
        ] {
            assert!(parse(raw.as_bytes()).is_err(), "{raw}");
        }
        assert!(parse(format!("{}0{}", "[".repeat(66), "]".repeat(66)).as_bytes()).is_err());
        for raw in [
            "[null,true,false,-1.5e+2]",
            r#""\ud83d\ude00""#,
            r#"{"path":"a\\b\"c","unicode":"中文"}"#,
        ] {
            parse(raw.as_bytes()).unwrap();
        }
    }
}
