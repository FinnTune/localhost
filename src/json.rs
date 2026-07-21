use std::collections::HashMap;
use std::iter::Peekable;
use std::str::Chars;

#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(HashMap<String, JsonValue>),
}

impl JsonValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<JsonValue>> {
        match self {
            JsonValue::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(map) => map.get(key),
            _ => None,
        }
    }
}

pub fn parse(input: &str) -> Result<JsonValue, String> {
    let mut chars = input.chars().peekable();
    skip_whitespace(&mut chars);
    let value = parse_value(&mut chars)?;
    skip_whitespace(&mut chars);
    if chars.next().is_some() {
        return Err("unexpected trailing characters after JSON value".to_string());
    }
    Ok(value)
}

fn skip_whitespace(chars: &mut Peekable<Chars>) {
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
}

fn parse_value(chars: &mut Peekable<Chars>) -> Result<JsonValue, String> {
    skip_whitespace(chars);
    match chars.peek() {
        Some('"') => parse_string(chars).map(JsonValue::String),
        Some('{') => parse_object(chars),
        Some('[') => parse_array(chars),
        Some('t') | Some('f') => parse_bool(chars),
        Some('n') => parse_null(chars),
        Some(c) if c.is_ascii_digit() || *c == '-' => parse_number(chars),
        Some(c) => Err(format!("unexpected character '{}'", c)),
        None => Err("unexpected end of input".to_string()),
    }
}

fn expect(chars: &mut Peekable<Chars>, expected: char) -> Result<(), String> {
    match chars.next() {
        Some(c) if c == expected => Ok(()),
        Some(c) => Err(format!("expected '{}', found '{}'", expected, c)),
        None => Err(format!("expected '{}', found end of input", expected)),
    }
}

fn parse_literal(chars: &mut Peekable<Chars>, literal: &str) -> Result<(), String> {
    for expected in literal.chars() {
        expect(chars, expected)?;
    }
    Ok(())
}

fn parse_null(chars: &mut Peekable<Chars>) -> Result<JsonValue, String> {
    parse_literal(chars, "null")?;
    Ok(JsonValue::Null)
}

fn parse_bool(chars: &mut Peekable<Chars>) -> Result<JsonValue, String> {
    match chars.peek() {
        Some('t') => {
            parse_literal(chars, "true")?;
            Ok(JsonValue::Bool(true))
        }
        Some('f') => {
            parse_literal(chars, "false")?;
            Ok(JsonValue::Bool(false))
        }
        _ => Err("expected boolean literal".to_string()),
    }
}

fn parse_number(chars: &mut Peekable<Chars>) -> Result<JsonValue, String> {
    let mut raw = String::new();
    if let Some(&'-') = chars.peek() {
        raw.push('-');
        chars.next();
    }
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            raw.push(c);
            chars.next();
        } else {
            break;
        }
    }
    if let Some(&'.') = chars.peek() {
        raw.push('.');
        chars.next();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() {
                raw.push(c);
                chars.next();
            } else {
                break;
            }
        }
    }
    if let Some(&e) = chars.peek() {
        if e == 'e' || e == 'E' {
            raw.push(e);
            chars.next();
            if let Some(&sign) = chars.peek() {
                if sign == '+' || sign == '-' {
                    raw.push(sign);
                    chars.next();
                }
            }
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() {
                    raw.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
        }
    }
    raw.parse::<f64>()
        .map(JsonValue::Number)
        .map_err(|_| format!("invalid number literal '{}'", raw))
}

fn parse_string(chars: &mut Peekable<Chars>) -> Result<String, String> {
    expect(chars, '"')?;
    let mut result = String::new();
    loop {
        match chars.next() {
            Some('"') => return Ok(result),
            Some('\\') => match chars.next() {
                Some('"') => result.push('"'),
                Some('\\') => result.push('\\'),
                Some('/') => result.push('/'),
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('b') => result.push('\u{0008}'),
                Some('f') => result.push('\u{000C}'),
                Some('u') => {
                    let mut code = String::new();
                    for _ in 0..4 {
                        match chars.next() {
                            Some(c) => code.push(c),
                            None => return Err("truncated unicode escape".to_string()),
                        }
                    }
                    let code_point = u32::from_str_radix(&code, 16)
                        .map_err(|_| format!("invalid unicode escape '\\u{}'", code))?;
                    if let Some(ch) = char::from_u32(code_point) {
                        result.push(ch);
                    }
                }
                Some(c) => return Err(format!("invalid escape sequence '\\{}'", c)),
                None => return Err("truncated escape sequence".to_string()),
            },
            Some(c) => result.push(c),
            None => return Err("unterminated string literal".to_string()),
        }
    }
}

fn parse_array(chars: &mut Peekable<Chars>) -> Result<JsonValue, String> {
    expect(chars, '[')?;
    let mut items = Vec::new();
    skip_whitespace(chars);
    if let Some(&']') = chars.peek() {
        chars.next();
        return Ok(JsonValue::Array(items));
    }
    loop {
        let value = parse_value(chars)?;
        items.push(value);
        skip_whitespace(chars);
        match chars.next() {
            Some(',') => {
                skip_whitespace(chars);
                continue;
            }
            Some(']') => return Ok(JsonValue::Array(items)),
            Some(c) => return Err(format!("expected ',' or ']', found '{}'", c)),
            None => return Err("unterminated array".to_string()),
        }
    }
}

fn parse_object(chars: &mut Peekable<Chars>) -> Result<JsonValue, String> {
    expect(chars, '{')?;
    let mut map = HashMap::new();
    skip_whitespace(chars);
    if let Some(&'}') = chars.peek() {
        chars.next();
        return Ok(JsonValue::Object(map));
    }
    loop {
        skip_whitespace(chars);
        let key = parse_string(chars)?;
        skip_whitespace(chars);
        expect(chars, ':')?;
        let value = parse_value(chars)?;
        map.insert(key, value);
        skip_whitespace(chars);
        match chars.next() {
            Some(',') => continue,
            Some('}') => return Ok(JsonValue::Object(map)),
            Some(c) => return Err(format!("expected ',' or '}}', found '{}'", c)),
            None => return Err("unterminated object".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars() {
        assert_eq!(parse("null").unwrap(), JsonValue::Null);
        assert_eq!(parse("true").unwrap(), JsonValue::Bool(true));
        assert_eq!(parse("false").unwrap(), JsonValue::Bool(false));
        assert_eq!(parse("42").unwrap(), JsonValue::Number(42.0));
        assert_eq!(parse("-3.5e2").unwrap(), JsonValue::Number(-350.0));
        assert_eq!(
            parse("\"hi\\n\"").unwrap(),
            JsonValue::String("hi\n".to_string())
        );
    }

    #[test]
    fn parses_nested_structures() {
        let input = r#"{"servers": [{"address": "127.0.0.1:8080", "endpoints": ["/"]}]}"#;
        let value = parse(input).unwrap();
        let servers = value.get("servers").unwrap().as_array().unwrap();
        assert_eq!(servers.len(), 1);
        let address = servers[0].get("address").unwrap().as_str().unwrap();
        assert_eq!(address, "127.0.0.1:8080");
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(parse("42 garbage").is_err());
    }
}
