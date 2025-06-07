use color_eyre::Result;
use std::path::{Path, PathBuf};

pub fn find_available_path<P: AsRef<Path>>(original_path: P) -> Result<PathBuf> {
    let original_path = original_path.as_ref();
    let stem = original_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let extension = original_path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{}", s))
        .unwrap_or_default();

    let parent = original_path.parent().unwrap_or_else(|| Path::new("."));

    for i in 1.. {
        let new_filename = format!("{} ({}){}", stem, i, extension);
        let new_path = parent.join(new_filename);

        if !new_path.try_exists()? {
            return Ok(new_path);
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_find_available_path() {
        // 创建一个临时文件
        let temp_file = NamedTempFile::new().unwrap();
        let original_path = temp_file.path();

        // 测试已存在文件的情况
        let new_path = find_available_path(original_path).unwrap();
        assert_eq!(
            new_path.file_name().unwrap().to_str().unwrap(),
            format!(
                "{} (1)",
                original_path.file_name().unwrap().to_str().unwrap()
            )
        );

        // 测试不存在的文件
        let non_existent = Path::new("nonexistent.txt");
        assert_eq!(find_available_path(non_existent).unwrap(), non_existent);
    }
}
