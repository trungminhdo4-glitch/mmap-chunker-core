//! Minimal strict JSON reader for mmap-chunker's own artifacts.
//!
//! The crate is zero-dependency by contract, so manifest verification
//! and index loading need a small, bounded JSON parser. It supports the
//! full value grammar (objects, arrays, strings with escapes, numbers,
//! booleans, null), enforces a nesting limit, and rejects trailing data.
//!
//! Numbers are stored as their source text and converted on access, so
//! large `u64`/`u128` values (byte offsets, nanosecond timestamps) round
//! trip without precision loss.

use std::fmt;

/// Maximum nesting depth accepted by the parser.
pub const MAX_DEPTH: usize = 64;

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// Number kept as source text for exact integer conversion.
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// Parse error with a byte position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    pub position: usize,
    pub message: &'static str,
}

impl fmt::Display for JsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid JSON at byte {}: {}",
            self.position, self.message
        )
    }
}

impl std::error::Error for JsonError {}

impl Json {
    /// Parse a complete JSON document.
    pub fn parse(text: &str) -> Result<Self, JsonError> {
        let mut parser = Parser {
            bytes: text.as_bytes(),
            position: 0,
        };
        parser.skip_whitespace();
        let value = parser.parse_value(0)?;
        parser.skip_whitespace();
        if parser.position != parser.bytes.len() {
            return Err(parser.error("trailing data after JSON value"));
        }
        Ok(value)
    }

    /// Object member lookup.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Object(members) => members
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// Array elements, if this value is an array.
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    /// String value.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    /// Boolean value.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// Unsigned integer value; rejects fractions and exponents.
    pub fn as_u64(&self) -> Option<u64> {
        let text = match self {
            Self::Number(text) => text,
            _ => return None,
        };
        if text.contains(['.', 'e', 'E']) || text.starts_with('-') {
            return None;
        }
        text.parse::<u64>().ok()
    }

    /// Unsigned 128-bit integer value; rejects fractions and exponents.
    pub fn as_u128(&self) -> Option<u128> {
        let text = match self {
            Self::Number(text) => text,
            _ => return None,
        };
        if text.contains(['.', 'e', 'E']) || text.starts_with('-') {
            return None;
        }
        text.parse::<u128>().ok()
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl Parser<'_> {
    fn error(&self, message: &'static str) -> JsonError {
        JsonError {
            position: self.position,
            message,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.position += 1;
        Some(byte)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Json, JsonError> {
        if depth > MAX_DEPTH {
            return Err(self.error("maximum nesting depth exceeded"));
        }
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => Ok(Json::String(self.parse_string()?)),
            Some(b't') => self.parse_literal("true", Json::Bool(true)),
            Some(b'f') => self.parse_literal("false", Json::Bool(false)),
            Some(b'n') => self.parse_literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn parse_literal(&mut self, literal: &str, value: Json) -> Result<Json, JsonError> {
        if self.bytes[self.position..].starts_with(literal.as_bytes()) {
            self.position += literal.len();
            Ok(value)
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.position += 1;
        let mut members = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.position += 1;
            return Ok(Json::Object(members));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("object key must be a string"));
            }
            let key = self.parse_string()?;
            self.skip_whitespace();
            if self.next() != Some(b':') {
                return Err(self.error("expected ':' after object key"));
            }
            self.skip_whitespace();
            let value = self.parse_value(depth + 1)?;
            members.push((key, value));
            self.skip_whitespace();
            match self.next() {
                Some(b',') => continue,
                Some(b'}') => return Ok(Json::Object(members)),
                _ => return Err(self.error("expected ',' or '}' in object")),
            }
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.position += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.position += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.parse_value(depth + 1)?);
            self.skip_whitespace();
            match self.next() {
                Some(b',') => continue,
                Some(b']') => return Ok(Json::Array(items)),
                _ => return Err(self.error("expected ',' or ']' in array")),
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        if self.next() != Some(b'"') {
            return Err(self.error("expected string"));
        }
        let mut out = String::new();
        loop {
            let byte = self
                .next()
                .ok_or_else(|| self.error("unterminated string"))?;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let escape = self
                        .next()
                        .ok_or_else(|| self.error("unterminated escape"))?;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.parse_unicode_escape()?),
                        _ => return Err(self.error("invalid escape sequence")),
                    }
                }
                control if control < 0x20 => {
                    return Err(self.error("unescaped control character in string"));
                }
                ascii if ascii < 0x80 => out.push(char::from(ascii)),
                _ => {
                    // Re-decode the UTF-8 sequence from the source text.
                    let start = self.position - 1;
                    let rest = std::str::from_utf8(&self.bytes[start..])
                        .map_err(|_| self.error("invalid UTF-8 in string"))?;
                    let character = rest
                        .chars()
                        .next()
                        .ok_or_else(|| self.error("invalid UTF-8 in string"))?;
                    self.position = start + character.len_utf8();
                    out.push(character);
                }
            }
        }
    }

    fn parse_unicode_escape(&mut self) -> Result<char, JsonError> {
        let high = self.parse_hex4()?;
        if (0xD800..=0xDBFF).contains(&high) {
            if self.next() != Some(b'\\') || self.next() != Some(b'u') {
                return Err(self.error("unpaired high surrogate"));
            }
            let low = self.parse_hex4()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(self.error("invalid low surrogate"));
            }
            let combined =
                0x1_0000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
            return char::from_u32(combined).ok_or_else(|| self.error("invalid surrogate pair"));
        }
        if (0xDC00..=0xDFFF).contains(&high) {
            return Err(self.error("unpaired low surrogate"));
        }
        char::from_u32(u32::from(high)).ok_or_else(|| self.error("invalid unicode escape"))
    }

    fn parse_hex4(&mut self) -> Result<u16, JsonError> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let byte = self
                .next()
                .ok_or_else(|| self.error("truncated \\u escape"))?;
            let digit = (byte as char)
                .to_digit(16)
                .ok_or_else(|| self.error("invalid hex digit in \\u escape"))?;
            value = (value << 4) | (digit as u16);
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<Json, JsonError> {
        let start = self.position;
        if self.peek() == Some(b'-') {
            self.position += 1;
        }
        match self.peek() {
            Some(b'0') => self.position += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.position += 1;
                }
            }
            _ => return Err(self.error("invalid number")),
        }
        if self.peek() == Some(b'.') {
            self.position += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("invalid fraction"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.position += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("invalid exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.position += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.position])
            .map_err(|_| self.error("invalid number encoding"))?;
        Ok(Json::Number(text.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_values() {
        let value = Json::parse(
            r#"{"a": [1, 2.5, -3e2, true, false, null], "b": {"c": "x\u0041\u00e9\ud83d\ude00"}}"#,
        )
        .unwrap();

        assert_eq!(value.get("a").unwrap().as_array().unwrap().len(), 6);
        assert_eq!(
            value.get("a").unwrap().as_array().unwrap()[0].as_u64(),
            Some(1)
        );
        assert_eq!(
            value.get("a").unwrap().as_array().unwrap()[1].as_u64(),
            None
        );
        assert_eq!(
            value.get("a").unwrap().as_array().unwrap()[3].as_bool(),
            Some(true)
        );
        assert_eq!(
            value.get("b").unwrap().get("c").unwrap().as_str(),
            Some("xA\u{e9}\u{1f600}")
        );
    }

    #[test]
    fn large_integers_round_trip_as_text() {
        let value =
            Json::parse(r#"{"mtime": 1790008126888781900, "offset": 18446744073709551615}"#)
                .unwrap();
        assert_eq!(
            value.get("mtime").unwrap().as_u128(),
            Some(1_790_008_126_888_781_900)
        );
        assert_eq!(value.get("offset").unwrap().as_u64(), Some(u64::MAX));
    }

    #[test]
    fn rejects_malformed_documents() {
        let cases = [
            "",
            "{",
            "{\"a\":}",
            "{\"a\" 1}",
            "[1,]",
            "{\"a\":1,}",
            "tru",
            "01",
            "1.",
            "1e",
            "\"unterminated",
            "\"bad\\xescape\"",
            "{} trailing",
        ];
        for case in cases {
            assert!(
                Json::parse(case).is_err(),
                "case unexpectedly parsed: {case}"
            );
        }
    }

    #[test]
    fn rejects_excessive_nesting() {
        let depth = MAX_DEPTH + 2;
        let document = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        assert!(Json::parse(&document).is_err());
    }

    #[test]
    fn rejects_unescaped_control_characters() {
        assert!(Json::parse("\"line\nbreak\"").is_err());
    }
}
