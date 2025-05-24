use fast_down::Progress;

pub fn fmt_progress(progress: &[Progress]) -> String {
    progress
        .iter()
        .map(|p| format!("{}-{}", p.start, p.end - 1))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_progress() {
        let progress = vec![0..10, 20..30];
        assert_eq!(fmt_progress(&progress), "0-9,20-29");
    }

    #[test]
    fn test_display_progress_empty() {
        let progress: Vec<Progress> = vec![];
        assert_eq!(fmt_progress(&progress), "");
    }

    #[test]
    fn test_display_progress_single() {
        let progress = vec![5..15];
        assert_eq!(fmt_progress(&progress), "5-14");
    }
}
