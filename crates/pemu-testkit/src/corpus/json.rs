//! The smallest JSON reader for the corpus manifest: an array of flat objects of strings,
//! numbers, booleans or null. `pemu-testkit` may use no third-party crate, so no `serde_json`. A
//! nested value is rejected, since a manifest that grew one would need a reader change anyway.

use std::collections::BTreeMap;
use std::fmt;

/// One scalar JSON value of a manifest object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scalar {
    Str(String),
    /// A number, kept as written: manifest numbers are byte sizes and fit in `u64`.
    Num(String),
    Bool(bool),
    Null,
}

impl Scalar {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Scalar::Str(text) => Some(text),
            _ => None,
        }
    }

    /// The number as `u64`, or `None` when it is not an integer that fits.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Scalar::Num(text) => text.parse().ok(),
            _ => None,
        }
    }
}

/// A parse failure, with the byte offset it was found at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError {
    pub at: usize,
    pub message: String,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.message, self.at)
    }
}

pub fn parse_object_array(text: &str) -> Result<Vec<BTreeMap<String, Scalar>>, JsonError> {
    let mut p = Parser {
        bytes: text.as_bytes(),
        at: 0,
    };
    p.skip_ws();
    p.expect(b'[')?;
    let mut out = Vec::new();
    p.skip_ws();
    if p.peek() == Some(b']') {
        p.at += 1;
        p.skip_ws();
        return p.end(out);
    }
    loop {
        p.skip_ws();
        out.push(p.object()?);
        p.skip_ws();
        match p.peek() {
            Some(b',') => p.at += 1,
            Some(b']') => {
                p.at += 1;
                break;
            }
            _ => return Err(p.err("expected `,` or `]`")),
        }
    }
    p.skip_ws();
    p.end(out)
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn err(&self, message: &str) -> JsonError {
        JsonError {
            at: self.at,
            message: message.to_string(),
        }
    }

    fn end<T>(&self, value: T) -> Result<T, JsonError> {
        if self.at == self.bytes.len() {
            Ok(value)
        } else {
            Err(self.err("trailing text after the array"))
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), JsonError> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected `{}`", byte as char)))
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn object(&mut self) -> Result<BTreeMap<String, Scalar>, JsonError> {
        self.expect(b'{')?;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(map);
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let value = self.scalar()?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(map);
                }
                _ => return Err(self.err("expected `,` or `}`")),
            }
        }
    }

    fn scalar(&mut self) -> Result<Scalar, JsonError> {
        match self.peek() {
            Some(b'"') => Ok(Scalar::Str(self.string()?)),
            Some(b't') => self.literal("true").map(|()| Scalar::Bool(true)),
            Some(b'f') => self.literal("false").map(|()| Scalar::Bool(false)),
            Some(b'n') => self.literal("null").map(|()| Scalar::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(self.err("expected a string, number, boolean or null")),
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), JsonError> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(())
        } else {
            Err(self.err(&format!("expected `{word}`")))
        }
    }

    fn number(&mut self) -> Result<Scalar, JsonError> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || c == b'+' || c == b'-')
        {
            self.at += 1;
        }
        if self.at == start {
            return Err(self.err("expected a number"));
        }
        Ok(Scalar::Num(
            String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned(),
        ))
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.err("unterminated string"));
            };
            self.at += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(esc) = self.peek() else {
                        return Err(self.err("unterminated escape"));
                    };
                    self.at += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(self.err("unknown escape")),
                    }
                }
                _ => {
                    // Multi-byte UTF-8 passes through: take the whole character from the text.
                    let start = self.at - 1;
                    let len = utf8_len(byte);
                    self.at = start + len;
                    let slice = self
                        .bytes
                        .get(start..self.at)
                        .ok_or_else(|| self.err("truncated UTF-8"))?;
                    out.push_str(&String::from_utf8_lossy(slice));
                }
            }
        }
    }

    /// A `\uXXXX` escape. Surrogate pairs are not joined; a lone or paired surrogate becomes the
    /// replacement character, which is enough for a manifest of file names.
    fn unicode_escape(&mut self) -> Result<char, JsonError> {
        let hex = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or_else(|| self.err("short \\u escape"))?;
        let text = core::str::from_utf8(hex).map_err(|_| self.err("bad \\u escape"))?;
        let value = u32::from_str_radix(text, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.at += 4;
        Ok(char::from_u32(value).unwrap_or(char::REPLACEMENT_CHARACTER))
    }
}

/// Length in bytes of the UTF-8 character starting with `lead`.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}
