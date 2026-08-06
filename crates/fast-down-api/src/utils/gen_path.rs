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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::PartialConfig;
    use fast_down::UrlInfo;
    use inherit_config::ConfigLayer;
    use std::path::PathBuf;
    use url::Url;

    fn make_info(raw_name: &str, content_type: Option<&str>) -> UrlInfo {
        UrlInfo {
            size: 100,
            raw_name: raw_name.to_string(),
            supports_range: true,
            fast_download: true,
            final_url: Url::parse("https://example.com/x").unwrap(),
            file_id: fast_down::FileId::new(None, None),
            content_type: content_type.map(str::to_string),
        }
    }

    fn make_config(save_dir: &std::path::Path, filename: &str, parse_filename: bool) -> Config {
        let pc = PartialConfig {
            save_dir: Some(save_dir.to_path_buf()),
            filename: Some(filename.to_string()),
            parse_filename: Some(parse_filename),
            ..Default::default()
        };
        pc.build()
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gen_path_test_{name}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn empty_filename_uses_auto_ext() {
        // filename empty => `auto_ext` branch (line 12) is taken.
        let dir = tempdir("auto_ext");
        let url = Url::parse("https://example.com/path/data.bin?x=1").unwrap();
        let info = make_info("data.bin", Some("application/octet-stream"));
        let cfg = make_config(&dir, "", false);
        let p = gen_path(&url, &info, &cfg).await.unwrap();
        assert_eq!(p.file_name().unwrap(), "data.bin");
    }

    #[tokio::test]
    async fn explicit_filename_used_when_parse_disabled() {
        // filename non-empty + parse_filename false => `Cow::Borrowed` branch.
        let dir = tempdir("borrowed");
        let url = Url::parse("https://example.com/path/data.bin").unwrap();
        let info = make_info("data.bin", None);
        let cfg = make_config(&dir, "myname.txt", false);
        let p = gen_path(&url, &info, &cfg).await.unwrap();
        assert_eq!(p.file_name().unwrap(), "myname.txt");
    }

    #[tokio::test]
    async fn template_expands_into_subdir() {
        // parse_filename true + non-empty filename => template branch (lines 20-33).
        let dir = tempdir("template");
        let url = Url::parse("https://example.com/a/b/data.bin").unwrap();
        let info = make_info("data.bin", None);
        let cfg = make_config(&dir, "{parent_path}/{file_name}", true);
        let p = gen_path(&url, &info, &cfg).await.unwrap();
        // parent_path of /a/b/data.bin is "a/b", so the resolved path ends with it.
        assert!(p.ends_with("a/b/data.bin"), "unexpected path: {p:?}");
        // The synthesized parent directory must have been created by gen_path.
        assert!(p.parent().is_some_and(std::path::Path::exists));
    }

    #[tokio::test]
    async fn traversal_in_template_cannot_escape_save_dir() {
        // A template that tries to climb out of `save_dir` with `..` must never
        // resolve outside it. `sanitize_path` strips the `..` components, and the
        // `starts_with(save_dir)` guard rejects any join that would escape, so the
        // resolved path always stays inside the configured directory (it may
        // create an `etc/pwned` subdir *within* save_dir, but it can never reach
        // an ancestor of save_dir). `soft_canonicalize` returns a verbatim
        // (`\\?\`) path on Windows, so normalize that prefix before comparing.
        let dir = tempdir("traversal");
        let url = Url::parse("https://example.com/a/b/data.bin").unwrap();
        let info = make_info("data.bin", None);
        let cfg = make_config(&dir, "../../etc/pwned/{file_name}", true);
        let p = gen_path(&url, &info, &cfg).await.unwrap();

        let norm = |p: &std::path::Path| -> String {
            let s = p.to_string_lossy();
            s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
        };
        let p_norm = norm(&p);
        // `gen_path` canonicalizes `save_dir` via `soft_canonicalize`, which
        // follows symlinks (e.g. macOS `/var` -> `/private/var`). Canonicalize
        // `dir` the same way so the prefix check matches on every platform.
        let dir_norm = norm(&soft_canonicalize(&dir).unwrap());
        assert!(
            p_norm.starts_with(dir_norm.as_str()),
            "a traversal template must never resolve outside save_dir, got {p_norm}"
        );
        assert!(
            p.ends_with("data.bin"),
            "the file name must be the template's leaf, not the traversal target, got {p:?}"
        );
    }
}
