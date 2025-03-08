use crate::download_progress::DownloadProgress;

pub fn merge_progress(progress: &mut Vec<DownloadProgress>, new: DownloadProgress) {
    let i = progress.partition_point(|old| old.start < new.start);
    if i == progress.len() {
        match progress.last_mut() {
            Some(last) if last.end >= new.start - 1 => {
                last.end = last.end.max(new.end);
            }
            _ => progress.push(new),
        }
    } else {
        let u1 = if i == 0 {
            false
        } else {
            progress[i - 1].can_merge(&new)
        };
        let u2 = progress[i].can_merge(&new);
        if u1 && u2 {
            progress[i - 1].end = progress[i].end;
            progress.remove(i);
        } else if u1 {
            progress[i - 1].end = new.end;
        } else if u2 {
            progress[i].start = new.start;
        } else {
            progress.insert(i, new);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_progress() {
        let mut progresses = vec![];
        merge_progress(&mut progresses, DownloadProgress::new(50, 60));
        assert_eq!(progresses, vec![DownloadProgress::new(50, 60)]);

        merge_progress(&mut progresses, DownloadProgress::new(10, 20));
        assert_eq!(
            progresses,
            vec![DownloadProgress::new(10, 20), DownloadProgress::new(50, 60)]
        );

        merge_progress(&mut progresses, DownloadProgress::new(30, 40));
        assert_eq!(
            progresses,
            vec![
                DownloadProgress::new(10, 20),
                DownloadProgress::new(30, 40),
                DownloadProgress::new(50, 60)
            ]
        );

        merge_progress(&mut progresses, DownloadProgress::new(41, 50));
        assert_eq!(
            progresses,
            vec![DownloadProgress::new(10, 20), DownloadProgress::new(30, 60)]
        );

        merge_progress(&mut progresses, DownloadProgress::new(61, 70));
        assert_eq!(
            progresses,
            vec![DownloadProgress::new(10, 20), DownloadProgress::new(30, 70)]
        );

        merge_progress(&mut progresses, DownloadProgress::new(80, 90));
        assert_eq!(
            progresses,
            vec![
                DownloadProgress::new(10, 20),
                DownloadProgress::new(30, 70),
                DownloadProgress::new(80, 90)
            ]
        );

        merge_progress(&mut progresses, DownloadProgress::new(70, 79));
        assert_eq!(
            progresses,
            vec![DownloadProgress::new(10, 20), DownloadProgress::new(30, 90)]
        );

        merge_progress(&mut progresses, DownloadProgress::new(0, 9));
        assert_eq!(
            progresses,
            vec![DownloadProgress::new(0, 20), DownloadProgress::new(30, 90)]
        );

        merge_progress(&mut progresses, DownloadProgress::new(20, 30));
        assert_eq!(progresses, vec![DownloadProgress::new(0, 90)]);

        merge_progress(&mut progresses, DownloadProgress::new(100, 110));
        assert_eq!(
            progresses,
            vec![
                DownloadProgress::new(0, 90),
                DownloadProgress::new(100, 110)
            ]
        );

        merge_progress(&mut progresses, DownloadProgress::new(91, 99));
        assert_eq!(progresses, vec![DownloadProgress::new(0, 110)])
    }
}
