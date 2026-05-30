use std::{iter::Peekable, str::Chars};

#[derive(Debug, PartialEq, Eq)]
pub struct ContentDisposition {
    pub filename: Option<String>,
}
impl ContentDisposition {
    #[must_use]
    pub fn parse(header_value: &str) -> Self {
        let mut filename = None;
        let mut filename_star = None;
        // 1. Skip the disposition-type (e.g. "attachment")
        // If there's no semicolon, there are no parameters
        let rest = match header_value.find(';') {
            Some(idx) => &header_value[idx + 1..],
            None => return Self { filename: None },
        };
        let mut chars = rest.chars().peekable();
        while chars.peek().is_some() {
            Self::consume_whitespace(&mut chars);
            // Read the key
            let key = Self::read_key(&mut chars);
            if key.is_empty() {
                // Handle consecutive semicolons (e.g. ";;")
                match chars.peek() {
                    Some(';') => {
                        chars.next();
                        continue;
                    }
                    _ => break,
                }
            }
            // Check for the separator after the key
            match chars.peek() {
                Some('=') => {
                    chars.next(); // consume '='
                }
                Some(';') => {
                    // Key is immediately followed by `;`, so it's a flag parameter (e.g. "hidden;")
                    // Skip this parameter and continue to the next one
                    chars.next();
                    continue;
                }
                None => break, // end of string
                _ => {
                    // Invalid character encountered; skip to the next semicolon to recover
                    Self::skip_until(&mut chars, ';');
                    continue;
                }
            }
            Self::consume_whitespace(&mut chars);
            // Read the value
            let value = match chars.peek() {
                Some('"') => {
                    chars.next(); // consume the opening quote
                    Self::read_quoted_string(&mut chars)
                }
                _ => Self::read_token(&mut chars),
            };
            // Consume the semicolon if it follows the value
            // Note: if read_token stopped due to whitespace, skip spaces to find the semicolon
            Self::consume_whitespace(&mut chars);
            if matches!(chars.peek(), Some(';')) {
                chars.next();
            }
            // Match the key
            if key.eq_ignore_ascii_case("filename") {
                filename = Some(value);
            } else if key.eq_ignore_ascii_case("filename*") {
                filename_star = Self::parse_filename_star(&value);
            }
        }
        Self {
            filename: filename_star.or(filename),
        }
    }

    fn consume_whitespace(chars: &mut Peekable<Chars<'_>>) {
        while let Some(c) = chars.peek()
            && c.is_whitespace()
        {
            chars.next();
        }
    }

    /// Read a key, stopping at `=` or `;`
    fn read_key(chars: &mut Peekable<Chars<'_>>) -> String {
        let mut s = String::new();
        while let Some(&c) = chars.peek()
            && c != '='
            && c != ';'
        {
            s.push(c);
            chars.next();
        }
        s.trim().to_string()
    }

    /// Read an unquoted token (value)
    /// Stops at `;` or **whitespace**
    fn read_token(chars: &mut Peekable<Chars<'_>>) -> String {
        let mut s = String::new();
        while let Some(&c) = chars.peek()
            && c != ';'
            && !c.is_whitespace()
        {
            s.push(c);
            chars.next();
        }
        s
    }

    fn read_quoted_string(chars: &mut Peekable<Chars<'_>>) -> String {
        let mut s = String::new();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        s.push(escaped);
                    }
                }
                _ => s.push(c),
            }
        }
        s
    }

    fn skip_until(chars: &mut Peekable<Chars<'_>>, target: char) {
        for c in chars.by_ref() {
            if c == target {
                break;
            }
        }
    }

    fn parse_filename_star(val: &str) -> Option<String> {
        let mut parts = val.splitn(3, '\'');
        let charset = parts.next()?;
        parts.next()?;
        let encoded_text = parts.next()?;
        if charset.eq_ignore_ascii_case("UTF-8") {
            Self::percent_decode(encoded_text)
        } else {
            None
        }
    }

    fn percent_decode(s: &str) -> Option<String> {
        let mut bytes = Vec::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '%' {
                let h = chars.next()?.to_digit(16)?;
                let l = chars.next()?.to_digit(16)?;
                #[allow(clippy::cast_possible_truncation)]
                let byte = ((h as u8) << 4) | (l as u8);
                bytes.push(byte);
            } else {
                bytes.push(c as u8);
            }
        }
        String::from_utf8(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn test_multiple_params_no_semicolon() {
        let s = "attachment; filename=foo.txt size=10";
        let cd = ContentDisposition::parse(s);
        assert_eq!(cd.filename.unwrap(), "foo.txt");
    }

    #[test]
    fn test_quoted_with_spaces() {
        let s = r#"attachment; filename="foo\" bar.txt"; size=10"#;
        let cd = ContentDisposition::parse(s);
        assert_eq!(cd.filename.unwrap(), "foo\" bar.txt");
    }

    #[test]
    fn test_flag_parameter() {
        let s = r#"attachment; hidden; filename="test.txt""#;
        let cd = ContentDisposition::parse(s);
        assert_eq!(cd.filename.unwrap(), "test.txt");
    }

    #[test]
    fn test_complex_filename_star() {
        let s = "attachment; filename*=UTF-8''%E6%B5%8B%E8%AF%95.txt";
        let cd = ContentDisposition::parse(s);
        assert_eq!(cd.filename.unwrap(), "测试.txt");
        let s = r#"attachment; filename=";;;"; filename*=UTF-8''%E6%B5%8B%E8%AF%95.txt"#;
        let cd = ContentDisposition::parse(s);
        assert_eq!(cd.filename.unwrap(), "测试.txt");
    }

    #[test]
    fn test_empty_values() {
        let s = r#"attachment; filename=";\";;"; filename*=""#; // invalid filename* will be ignored
        let cd = ContentDisposition::parse(s);
        assert_eq!(cd.filename.unwrap(), ";\";;");
    }
}
