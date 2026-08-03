//! Parser for the HTTP `Content-Disposition` header.
//!
//! Used during prefetch to derive a suggested filename for a downloaded
//! resource. [`ContentDisposition::parse`] handles both the quoted `filename`
//! form and the RFC 5987 `filename*` (UTF-8, percent-encoded) form, with the
//! latter taking precedence when both are present.

use std::{iter::Peekable, str::Chars};

/// Parsed `Content-Disposition` header, extracting the `filename` parameter.
///
/// Supports both `filename="..."` (quoted) and `filename*=UTF-8''...` (RFC 5987)
/// encoding. When both are present, `filename*` takes precedence.
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

    #[test]
    fn test_no_semicolon_is_none() {
        assert_eq!(ContentDisposition::parse("attachment").filename, None);
    }

    #[test]
    fn test_empty_header() {
        assert_eq!(ContentDisposition::parse("").filename, None);
        assert_eq!(ContentDisposition::parse("   ").filename, None);
    }

    #[test]
    fn test_filename_star_non_utf8_ignored() {
        // A non-UTF-8 charset is unsupported, so `filename*` is dropped.
        let s = "attachment; filename*=ISO-8859-1''%A3.txt";
        assert_eq!(ContentDisposition::parse(s).filename, None);
    }

    #[test]
    fn test_filename_star_wins_over_filename() {
        let s = r#"attachment; filename="old.txt"; filename*=UTF-8''%E6%B5%8B.txt"#;
        assert_eq!(ContentDisposition::parse(s).filename.unwrap(), "测.txt");
    }

    #[test]
    fn test_unquoted_token_stops_at_space() {
        let s = "attachment; filename=foo bar.txt";
        assert_eq!(ContentDisposition::parse(s).filename.unwrap(), "foo");
    }

    #[test]
    fn test_consecutive_semicolons() {
        let s = "attachment;;; filename=foo.txt";
        assert_eq!(ContentDisposition::parse(s).filename.unwrap(), "foo.txt");
    }

    #[test]
    fn test_quoted_with_escaped_quote() {
        let s = r#"attachment; filename="a\"b.txt""#;
        assert_eq!(ContentDisposition::parse(s).filename.unwrap(), "a\"b.txt");
    }

    #[test]
    fn test_empty_key_followed_by_equals_breaks() {
        // `attachment;=x`: after the `;`, `read_key` yields an empty key and the
        // next character is `=` (neither `;` nor end-of-string), so the parser
        // breaks out of the loop (line 41) and extracts no filename.
        assert_eq!(ContentDisposition::parse("attachment;=x").filename, None);
        // Same path when the empty-key-then-`=` occurs mid-header.
        assert_eq!(
            ContentDisposition::parse("attachment;=x; filename=ok.txt").filename,
            None
        );
    }

    #[test]
    fn filename_star_raw_non_ascii_single_char_truncates_to_garbage() {
        // `percent_decode` pushes `c as u8` for non-'%' chars. U+6D4B '测'
        // truncates to 0x4B ('K'), which is valid ASCII, so the result is the
        // garbage "K" rather than a decode failure. RFC 5987 requires
        // percent-encoding, so this input is invalid; the current parser does
        // not reject it gracefully.
        let s = "attachment; filename*=UTF-8''测";
        assert_eq!(ContentDisposition::parse(s).filename.as_deref(), Some("K"));
    }

    #[test]
    fn filename_star_raw_non_ascii_invalid_utf8_is_dropped() {
        // '测' -> 0x4B, '试' -> 0xD5; 0xD5 is not a valid UTF-8 leading byte for
        // the bytes that follow, so `from_utf8` fails and `filename*` is dropped.
        let s = "attachment; filename*=UTF-8''测试";
        assert_eq!(ContentDisposition::parse(s).filename, None);
    }

    #[test]
    fn filename_star_empty_value_overrides_filename_with_empty() {
        // `filename*=UTF-8''` (empty encoded text) parses to Some(""), and
        // `filename_star.or(filename)` prefers it over a valid `filename`,
        // yielding an empty filename. (`prefetch::get_filename` filters empty
        // names, so the higher layer is unaffected.)
        let s = r#"attachment; filename="fallback.txt"; filename*=UTF-8''"#;
        assert_eq!(ContentDisposition::parse(s).filename.as_deref(), Some(""));
    }

    #[test]
    fn test_filename_star_single_decode_is_correct() {
        // The parser percent-decodes `filename*` exactly once: `%25100`
        // (the server-intended literal "%100") decodes to "%100". The
        // double-decode bug lives in `prefetch::get_filename`, not here.
        let s = "attachment; filename*=UTF-8''%25100";
        assert_eq!(
            ContentDisposition::parse(s).filename,
            Some("%100".to_string())
        );
    }

    #[test]
    fn test_filename_star_literal_multibyte_truncated() {
        // Known limitation (hypothesis B): `percent_decode` pushes non-'%'
        // chars with `c as u8`, truncating multibyte UTF-8. A malformed
        // `filename*` mixing a percent-encoded byte with a literal multibyte
        // char ("%41" + "é") yields invalid UTF-8 and the value is dropped
        // (None), falling back to the URL path/host name. RFC 5987 requires
        // `filename*` to be fully percent-encoded, so literal non-ASCII is
        // malformed input; dropping it is the defensible behavior. Kept as a
        // regression guard: a future fix that decodes correctly must update
        // this assertion.
        let s = "attachment; filename*=UTF-8''%41é";
        assert_eq!(ContentDisposition::parse(s).filename, None);
    }
}
