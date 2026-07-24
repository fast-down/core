use crate::{Config, utils::parse_filename_template};
use fast_down::UrlInfo;
use path_helper::{auto_ext, sanitize_filename, sanitize_path};
use soft_canonicalize::soft_canonicalize;
use std::{borrow::Cow, path::PathBuf};
use tokio::fs;
use url::Url;

pub async fn gen_path(url: &Url, info: &UrlInfo, config: &Config) -> std::io::Result<PathBuf> {
    let mut filename = sanitize_filename(
        if config.filename.is_empty() || config.parse_filename {
            auto_ext(&info.raw_name, info.content_type.as_deref())
        } else {
            Cow::Borrowed(config.filename.as_str())
        },
        248,
    );
    let mut save_dir = soft_canonicalize::soft_canonicalize(&config.save_dir)?;
    if config.parse_filename && !config.filename.is_empty() {
        let path = PathBuf::from(parse_filename_template(
            config.filename.clone(),
            url,
            &filename,
        ));
        if let Some(s) = path.file_name() {
            filename = sanitize_filename(s.to_string_lossy(), 248);
        }
        if let Some(parent_path) = path.parent()
            && let Ok(new_save_dir) = soft_canonicalize(save_dir.join(sanitize_path(parent_path)))
            && new_save_dir.starts_with(&save_dir)
        {
            save_dir = new_save_dir;
        }
    }
    fs::create_dir_all(&save_dir).await?;
    Ok(save_dir.join(&filename))
}
