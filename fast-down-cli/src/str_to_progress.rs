use fast_down::Progress;

pub fn str_to_progress(text: String) -> Vec<Progress> {
    if text.is_empty() {
        return vec![];
    }
    text.split(',')
        .map(|p| p.splitn(2, '-').map(|n| n.parse().unwrap()).collect())
        .map(|p: Vec<usize>| p[0]..p[1] + 1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_str_to_progress() {
        assert_eq!(
            str_to_progress("1-10,20-30,40-50".to_string()),
            vec![1..11, 20..31, 40..51]
        );
        assert_eq!(str_to_progress("".to_string()), vec![] as Vec<Progress>);
        assert_eq!(str_to_progress("2-5".to_string()), vec![2..6]);
    }
}
