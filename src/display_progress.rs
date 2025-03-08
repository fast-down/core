use crate::download_progress::DownloadProgress;

pub fn display_progress(progress: &[DownloadProgress]) -> String {
    progress
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_progress() {
        let progress = vec![DownloadProgress::new(0, 10), DownloadProgress::new(20, 30)];
        assert_eq!(display_progress(&progress), "0-10,20-30");
    }

    #[test]
    fn test_display_progress_empty() {
        let progress: Vec<DownloadProgress> = vec![];
        assert_eq!(display_progress(&progress), "");
    }

    #[test]
    fn test_display_progress_single() {
        let progress = vec![DownloadProgress::new(5, 15)];
        assert_eq!(display_progress(&progress), "5-15");
    }
}
