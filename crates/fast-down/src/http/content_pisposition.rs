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
        // 1. 跳过 disposition-type (如 "attachment")
        // 如果没有分号，说明没有参数，直接返回
        let rest = match header_value.find(';') {
            Some(idx) => &header_value[idx + 1..],
            None => return Self { filename: None },
        };
        let mut chars = rest.chars().peekable();
        while chars.peek().is_some() {
            Self::consume_whitespace(&mut chars);
            // 读取 Key
            let key = Self::read_key(&mut chars);
            if key.is_empty() {
                // 处理连续分号情况 (e.g. ";;")
                match chars.peek() {
                    Some(';') => {
                        chars.next();
                        continue;
                    }
                    _ => break,
                }
            }
            // 检查 Key 后的分隔符
            match chars.peek() {
                Some('=') => {
                    chars.next(); // 消费 '='
                }
                Some(';') => {
                    // Key 后面直接跟分号，说明是 Flag 参数 (如 "hidden;")
                    // 忽略此参数，直接消费分号并进入下一次循环
                    chars.next();
                    continue;
                }
                None => break, // 字符串结束
                _ => {
                    // 遇到非法字符，尝试跳到下一个分号恢复
                    Self::skip_until(&mut chars, ';');
                    continue;
                }
            }
            Self::consume_whitespace(&mut chars);
            // 读取 Value
            let value = match chars.peek() {
                Some('"') => {
                    chars.next(); // 消费起始引号
                    Self::read_quoted_string(&mut chars)
                }
                _ => Self::read_token(&mut chars),
            };
            // 如果 Value 后面紧跟分号，消费它
            // 注意：如果 read_token 因为遇到空格停止，这里需要跳过可能的空格找到分号
            Self::consume_whitespace(&mut chars);
            if matches!(chars.peek(), Some(';')) {
                chars.next();
            }
            // 匹配 Key
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

    /// 读取 Key，遇到 `=` 或 `;` 停止
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

    /// 读取未加引号的 Token (Value)
    /// 停止条件：遇到 `;` 或者 **空白字符**
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
        let s = r#"attachment; filename=";\";;"; filename*=""#; // filename* 格式不对会被忽略
        let cd = ContentDisposition::parse(s);
        assert_eq!(cd.filename.unwrap(), ";\";;");
    }
}
