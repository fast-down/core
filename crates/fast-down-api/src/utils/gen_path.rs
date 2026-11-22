use crate::{Config, utils::parse_filename_template};
use fast_down::UrlInfo;
use path_helper::{auto_ext, comparable, sanitize_filename, sanitize_path};
use soft_canonicalize::soft_canonicalize;
use std::{borrow::Cow, path::PathBuf};
use url::Url;

pub fn gen_path(url: &Url, info: &UrlInfo, config: &Config) -> std::io::Result<PathBuf> {
    let name = if config.filename.is_empty() || config.parse_filename {
        auto_ext(&info.raw_name, info.content_type.as_deref())
    } else {
        Cow::Borrowed(config.filename.as_str())
    };
    gen_path_named(url, config, name.as_ref())
}

/// The output path for a configuration whose name is fixed without the remote.
///
/// A non-empty `filename` with [`Config::parse_filename`] off is used verbatim,
/// so its destination is known before the prefetch. `None` means the name (or
/// the directory it implies) depends on the response and only [`gen_path`] can
/// resolve it. This lets plan validation refuse a path collision without
/// touching the network.
pub fn gen_path_from_name(url: &Url, config: &Config) -> Option<std::io::Result<PathBuf>> {
    if config.filename.is_empty() || config.parse_filename {
        return None;
    }
    Some(gen_path_named(url, config, &config.filename))
}

fn gen_path_named(url: &Url, config: &Config, initial: &str) -> std::io::Result<PathBuf> {
    let mut filename = checked_filename(initial)?;
    let mut save_dir = soft_canonicalize::soft_canonicalize(&config.save_dir)?;
    if config.parse_filename && !config.filename.is_empty() {
        let path = PathBuf::from(parse_filename_template(&config.filename, url, &filename));
        if let Some(s) = path.file_name() {
            filename = checked_filename(s.to_string_lossy().as_ref())?;
        }
        if let Some(parent_path) = path.parent()
            && let Ok(new_save_dir) = soft_canonicalize(save_dir.join(sanitize_path(parent_path)))
            && comparable(&new_save_dir).starts_with(comparable(&save_dir))
        {
            save_dir = new_save_dir;
        }
    }
    Ok(save_dir.join(&filename))
}

/// Sanitize one output name and refuse a name that ends up naming nothing.
///
/// A name made only of whitespace, or one the platform reduces to nothing, would
/// otherwise leave the join with an empty component, making `final_path` equal
/// to `save_dir`: the `.part` would be renamed onto a directory, or onto the
/// state file's directory itself. Reported as `InvalidInput` so the caller sees
/// a bad configuration rather than a filesystem failure later.
///
/// The raw input is checked as well as the result: `sanitize_filename` maps a
/// blank name onto its replacement character (`_`), so `"   "` comes back
/// non-blank and only the input still shows the name was empty.
fn checked_filename(name: &str) -> std::io::Result<String> {
    if name.trim().is_empty() {
        return Err(blank_name(name));
    }
    let cleaned = sanitize_filename(name, 248);
    if cleaned.trim().is_empty() {
        return Err(blank_name(name));
    }
    Ok(cleaned)
}

/// The configuration error for an output name that names nothing.
fn blank_name(name: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("the output filename {name:?} is blank"),
    )
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
            range_probe: fast_down::RangeProbe::Supported,
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
        // filename empty => the `auto_ext` branch is taken.
        let dir = tempdir("auto_ext");
        let url = Url::parse("https://example.com/path/data.bin?x=1").unwrap();
        let info = make_info("data.bin", Some("application/octet-stream"));
        let cfg = make_config(&dir, "", false);
        let p = gen_path(&url, &info, &cfg).unwrap();
        assert_eq!(p.file_name().unwrap(), "data.bin");
    }

    #[tokio::test]
    async fn explicit_filename_used_when_parse_disabled() {
        // filename non-empty + parse_filename false => `Cow::Borrowed` branch.
        let dir = tempdir("borrowed");
        let url = Url::parse("https://example.com/path/data.bin").unwrap();
        let info = make_info("data.bin", None);
        let cfg = make_config(&dir, "myname.txt", false);
        let p = gen_path(&url, &info, &cfg).unwrap();
        assert_eq!(p.file_name().unwrap(), "myname.txt");
    }

    #[tokio::test]
    async fn whitespace_only_filename_is_refused() {
        // A blank name must not resolve to `save_dir` itself: the join would
        // leave the directory as the output, and `finalize` would then rename
        // the `.part` onto a directory.
        let dir = tempdir("blank_name");
        let url = Url::parse("https://example.com/path/data.bin").unwrap();
        let info = make_info("data.bin", None);
        let cfg = make_config(&dir, "   ", false);
        let err = gen_path(&url, &info, &cfg).expect_err("a blank filename must be refused");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "a blank name is a configuration error: {err}"
        );
        assert!(
            err.to_string().contains("is blank"),
            "the failure must name the problem, got: {err}"
        );
    }

    #[tokio::test]
    async fn template_that_sanitizes_to_nothing_is_refused() {
        // The same check applies on the template branch, where the leaf is
        // sanitized after expansion.
        let dir = tempdir("blank_template");
        let url = Url::parse("https://example.com/path/data.bin").unwrap();
        let info = make_info("data.bin", None);
        let cfg = make_config(&dir, " ", true);
        let err = gen_path(&url, &info, &cfg).expect_err("an empty template leaf must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn template_expands_into_subdir() {
        // parse_filename true + non-empty filename => the template branch.
        let dir = tempdir("template");
        let url = Url::parse("https://example.com/a/b/data.bin").unwrap();
        let info = make_info("data.bin", None);
        let cfg = make_config(&dir, "{parent_path}/{file_name}", true);
        let p = gen_path(&url, &info, &cfg).unwrap();
        // parent_path of /a/b/data.bin is "a/b", so the resolved path ends with it.
        assert!(p.ends_with("a/b/data.bin"), "unexpected path: {p:?}");
        // `gen_path` only computes the path; it must NOT create the directory.
        // Directory creation is the download executor's job (`claim_and_run`),
        // covered by the integration test `download_creates_template_subdir`
        // in tests/resume.rs.
        assert!(
            !p.parent().unwrap().exists(),
            "gen_path must not touch the filesystem"
        );
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
        let p = gen_path(&url, &info, &cfg).unwrap();

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

    #[cfg(windows)]
    #[tokio::test]
    async fn unix_style_save_dir_maps_to_rooted_path() {
        // Regression baseline for the path-mangling defect: a Git-Bash style
        // `/c/Users/...` save_dir is canonicalized as a rooted path under the
        // current drive (e.g. `E:\c\Users\...`) rather than `C:\Users\...`.
        // `gen_path` itself faithfully follows Windows path semantics; the fix
        // belongs in the CLI layer (translate `/x/...` -> `X:/...` before the
        // config is built). Keeping this test pins the current behavior so a
        // future change to `gen_path`'s path handling is caught.
        let dir = std::path::PathBuf::from("/c/Users/example/downloads");
        let url = Url::parse("https://example.com/a.bin").unwrap();
        let info = make_info("a.bin", None);
        let cfg = make_config(&dir, "a.bin", false);
        let p = gen_path(&url, &info, &cfg).unwrap();
        let s = p.to_string_lossy();
        assert!(
            s.contains("c\\Users") || s.contains("c/Users"),
            "expected a rooted `/c/...` mapping under the current drive, got {s}"
        );
    }
}
