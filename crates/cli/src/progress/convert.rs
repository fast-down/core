use fast_down::Progress;

pub fn to_string(progress: &[Progress]) -> String {
    progress
        .iter()
        .map(|p| format!("{}-{}", p.start, p.end - 1))
        .collect::<Vec<_>>()
        .join(",")
}

pub fn from_str(text: &str) -> Vec<Progress> {
    if text.is_empty() {
        return vec![];
    }
    text.split(',')
        .map(|p| p.splitn(2, '-').map(|n| n.parse().unwrap()).collect())
        .map(|p: Vec<u64>| p[0]..p[1] + 1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_str_to_progress() {
        assert_eq!(
            from_str("1-10,20-30,40-50"),
            vec![1..11, 20..31, 40..51]
        );
        assert_eq!(from_str(""), vec![] as Vec<Progress>);
        assert_eq!(from_str("2-5"), vec![2..6]);
    }

    #[test]
    fn test_display_progress() {
        let progress = vec![0..10, 20..30];
        assert_eq!(to_string(&progress), "0-9,20-29");
    }

    #[test]
    fn test_display_progress_empty() {
        let progress: Vec<Progress> = vec![];
        assert_eq!(to_string(&progress), "");
    }

    #[test]
    fn test_display_progress_single() {
        let progress = vec![5..15];
        assert_eq!(to_string(&progress), "5-14");
    }
}
