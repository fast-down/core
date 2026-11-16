use chrono::Local;
use chrono::format::{Item, StrftimeItems};
use path_helper::sanitize_filename;
use url::Url;

pub fn parse_filename_template(template: &str, url: &Url, filename: &str) -> String {
    let template = expand_time(template);
    let host = sanitize_filename(url.host_str().unwrap_or("unknown"), 255);
    let mut parent_path: Vec<_> = url
        .path_segments()
        .into_iter()
        .flat_map(|segments| {
            segments.map(|seg| {
                let decoded = urlencoding::decode_binary(seg.as_bytes());
                sanitize_filename(String::from_utf8_lossy(&decoded).as_ref(), 255)
            })
        })
        .collect();
    parent_path.pop();
    parent_path.retain(|segment| !segment.is_empty());
    let parent_path = if parent_path.is_empty() {
        ".".to_string()
    } else {
        parent_path.join(std::path::MAIN_SEPARATOR_STR)
    };
    let (file_stem, file_ext) = filename
        .rfind('.')
        .map_or((filename, ""), |pos| (&filename[..pos], &filename[pos..]));
    #[allow(clippy::literal_string_with_formatting_args)]
    template
        .replace("{host}", &host)
        .replace("{parent_path}", &parent_path)
        .replace("{file_name}", filename)
        .replace("{file_stem}", file_stem)
        .replace("{file_ext}", file_ext)
}

/// Expand chrono `%`-sequences in a filename template against local time.
///
/// # Invalid sequences
///
/// Expansion is all-or-nothing. The whole template is pre-scanned for
/// [`Item::Error`]; if any sequence is invalid (`%Q`, a lone trailing `%`), the
/// time pass is skipped entirely and `template` is returned unchanged, so a
/// malformed sequence stays literal instead of being dropped or partially
/// formatted. This failure is deliberately silent — there is no `Result`
/// channel for a caller to handle, and a filename template with a stray `%` is
/// still a usable name. Formatting it anyway would instead panic inside chrono,
/// print through the panic hook, and with `panic = "abort"` take the process
/// down.
///
/// Only the `%`-sequences are affected: `{...}` placeholders are substituted by
/// the caller after this function returns, so they still expand, and a `%`
/// carried by a placeholder's *value* is never re-read as a format spec.
fn expand_time(template: &str) -> String {
    if StrftimeItems::new(template).any(|item| matches!(item, Item::Error)) {
        return template.to_string();
    }
    Local::now().format(template).to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use url::Url;

    #[test]
    fn all_placeholders() {
        let url = Url::parse("https://example.com/path/to/file.txt").unwrap();
        let t = "{host}/{parent_path}/{file_name}_{file_stem}{file_ext}";
        let out = parse_filename_template(t, &url, "file.txt");
        assert!(out.starts_with("example.com"));
        assert!(out.contains("path"));
        assert!(out.contains("to"));
        assert!(out.ends_with("file.txt_file.txt"));
    }

    #[test]
    fn no_placeholders_passthrough() {
        let url = Url::parse("https://example.com/x").unwrap();
        assert_eq!(parse_filename_template("plain", &url, "f.txt"), "plain");
    }

    #[test]
    fn host_unknown_when_no_host() {
        let url = Url::parse("file:///etc/hosts").unwrap();
        assert_eq!(parse_filename_template("{host}", &url, "hosts"), "unknown");
    }

    #[test]
    fn parent_path_root_when_no_dir() {
        let url = Url::parse("https://example.com/file.txt").unwrap();
        assert_eq!(
            parse_filename_template("{parent_path}", &url, "file.txt"),
            "."
        );
    }

    #[test]
    fn file_ext_includes_dot() {
        let url = Url::parse("https://example.com/a/b.tar.gz").unwrap();
        let out = parse_filename_template("{file_stem}{file_ext}", &url, "b.tar.gz");
        assert_eq!(out, "b.tar.gz");
    }

    #[test]
    fn no_dot_file_has_empty_ext() {
        let url = Url::parse("https://example.com/README").unwrap();
        let out = parse_filename_template("{file_stem}|{file_ext}", &url, "README");
        assert_eq!(out, "README|");
    }

    #[test]
    fn parent_path_is_dot_for_cannot_be_a_base_url() {
        // `mailto:` URLs are cannot-be-a-base, so `path_segments()` is `None` and
        // the parent path collapses to ".".
        let url = Url::parse("mailto:foo@x").unwrap();
        let out = parse_filename_template("{parent_path}/{file_name}", &url, "foo.txt");
        assert_eq!(out, "./foo.txt");
    }

    #[test]
    fn strftime_year_expands_without_leaving_the_sequence() {
        // `%Y` must be consumed by the time pass, and the result must be a real
        // four-digit year rather than the literal spec. Asserting the shape —
        // not the current year — keeps this independent of the wall clock.
        let url = Url::parse("https://example.com/file.txt").unwrap();
        let out = parse_filename_template("%Y-%m-%d/{file_name}", &url, "file.txt");
        assert!(
            !out.contains('%'),
            "no %-sequence may survive expansion: {out}"
        );
        let (date, name) = out.split_once('/').expect("date and name are distinct");
        assert_eq!(name, "file.txt");
        let mut parts = date.split('-');
        let year = parts.next().unwrap();
        assert_eq!(year.len(), 4, "expected a four-digit year in {date}");
        assert!(
            year.bytes().all(|b| b.is_ascii_digit()),
            "expected digits in {date}"
        );
        assert_eq!(parts.count(), 2, "expected a month and a day in {date}");
    }

    #[test]
    fn placeholders_expand_after_time_expansion() {
        // The time pass runs on the template alone, before `{...}` substitution:
        // the template's `%Y` expands, while a `%Y` that arrives with the file
        // name stays literal.
        let url = Url::parse("https://example.com/file.txt").unwrap();
        let out = parse_filename_template("%Y/{file_name}", &url, "raw%Y.txt");
        assert!(!out.starts_with("%Y/"), "template %Y must expand: {out}");
        assert!(
            out.ends_with("/raw%Y.txt"),
            "the name's %Y must not be re-expanded: {out}"
        );
    }

    #[test]
    fn invalid_time_sequence_is_kept_literally() {
        // `%Q` is not a chrono spec; the template must survive verbatim instead
        // of panicking through the formatter.
        let url = Url::parse("https://example.com/file.txt").unwrap();
        let out = parse_filename_template("%Q/{file_name}", &url, "file.txt");
        assert_eq!(out, "%Q/file.txt");
    }

    #[test]
    fn trailing_percent_is_kept_literally() {
        // A lone `%` at the end is also an `Item::Error`: the time pass is
        // skipped as a whole, and the placeholder still expands.
        let url = Url::parse("https://example.com/file.txt").unwrap();
        let out = parse_filename_template("report_%/{file_name}", &url, "file.txt");
        assert_eq!(out, "report_%/file.txt");
    }
}
