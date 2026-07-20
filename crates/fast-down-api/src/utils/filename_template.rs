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
