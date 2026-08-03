use chrono::Local;
use path_helper::sanitize_filename;
use std::panic;
use url::Url;

pub fn parse_filename_template(template: String, url: &Url, filename: &str) -> String {
    let template =
        panic::catch_unwind(|| Local::now().format(&template).to_string()).unwrap_or(template);
    let host = sanitize_filename(url.host_str().unwrap_or("unknown"), 255);
    let mut parent_path: Vec<_> = url
        .path_segments()
        .into_iter()
        .flat_map(|segments| {
            segments.map(|seg| {
                let decoded = urlencoding::decode_binary(seg.as_bytes());
                sanitize_filename(String::from_utf8_lossy(&decoded), 255)
            })
        })
        .collect();
    parent_path.pop();
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use url::Url;

    #[test]
    fn all_placeholders() {
        let url = Url::parse("https://example.com/path/to/file.txt").unwrap();
        let t = "{host}/{parent_path}/{file_name}_{file_stem}{file_ext}";
        let out = parse_filename_template(t.to_string(), &url, "file.txt");
        assert!(out.starts_with("example.com"));
        assert!(out.contains("path"));
        assert!(out.contains("to"));
        assert!(out.ends_with("file.txt_file.txt"));
    }

    #[test]
    fn no_placeholders_passthrough() {
        let url = Url::parse("https://example.com/x").unwrap();
        assert_eq!(
            parse_filename_template("plain".to_string(), &url, "f.txt"),
            "plain"
        );
    }

    #[test]
    fn host_unknown_when_no_host() {
        let url = Url::parse("file:///etc/hosts").unwrap();
        assert_eq!(
            parse_filename_template("{host}".to_string(), &url, "hosts"),
            "unknown"
        );
    }

    #[test]
    fn parent_path_root_when_no_dir() {
        let url = Url::parse("https://example.com/file.txt").unwrap();
        assert_eq!(
            parse_filename_template("{parent_path}".to_string(), &url, "file.txt"),
            "."
        );
    }

    #[test]
    fn file_ext_includes_dot() {
        let url = Url::parse("https://example.com/a/b.tar.gz").unwrap();
        let out = parse_filename_template("{file_stem}{file_ext}".to_string(), &url, "b.tar.gz");
        assert_eq!(out, "b.tar.gz");
    }

    #[test]
    fn no_dot_file_has_empty_ext() {
        let url = Url::parse("https://example.com/README").unwrap();
        let out = parse_filename_template("{file_stem}|{file_ext}".to_string(), &url, "README");
        assert_eq!(out, "README|");
    }

    #[test]
    fn parent_path_is_dot_for_cannot_be_a_base_url() {
        // `mailto:` URLs are cannot-be-a-base, so `path_segments()` is `None` and
        // the parent path collapses to "." (filename_template.rs lines 10-25).
        let url = Url::parse("mailto:foo@x").unwrap();
        let out = parse_filename_template("{parent_path}/{file_name}".to_string(), &url, "foo.txt");
        assert_eq!(out, "./foo.txt");
    }

    #[test]
    fn chrono_format_expands_into_template() {
        // A leading `%Y` is a chrono format spec expanded by `Local::now().format`
        // before the `{...}` placeholders are substituted (filename_template.rs line 7).
        let url = Url::parse("https://example.com/file.txt").unwrap();
        let out = parse_filename_template("%Y/file.txt".to_string(), &url, "file.txt");
        let year = chrono::Local::now().format("%Y").to_string();
        assert_eq!(out, format!("{year}/file.txt"));
    }
}
