use std::path::{Path, PathBuf};
use tokio::{fs, io};

pub async fn gen_unique_path(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref();
    if !fs::try_exists(path).await? {
        return Ok(path.into());
    }
    let stem = path.file_stem().unwrap_or_default();
    let ext = path.extension().unwrap_or_default();
    for i in 1.. {
        let mut new_name = stem.to_os_string();
        new_name.push(" (");
        new_name.push(i.to_string());
        new_name.push(").");
        new_name.push(ext);
        let new_path = path.with_file_name(new_name);
        if !fs::try_exists(&new_path).await? {
            return Ok(new_path);
        }
    }
    unreachable!()
}
