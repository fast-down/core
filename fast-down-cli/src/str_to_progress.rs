use fast_down::Progress;

pub fn str_to_progress(text: String) -> Vec<Progress> {
    text.split(',')
        .map(|p| p.split('-').map(|n| n.parse().unwrap()).collect())
        .map(|p: Vec<usize>| p[0]..p[1] + 1)
        .collect()
}
