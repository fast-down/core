use crate::download_progress::DownloadProgress;

pub fn display_progress(progress: &[DownloadProgress]) -> String {
    let mut s = vec![];
    for p in progress {
        s.push(p.to_string());
    }
    s.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_progress() {
        let progress = vec![DownloadProgress::new(0, 10), DownloadProgress::new(20, 30)];
        assert_eq!(display_progress(&progress), "0-10,20-30");
    }
}
