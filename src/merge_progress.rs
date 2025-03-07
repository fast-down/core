use crate::download::DownloadProgress;

pub fn merge_progress(progress: &mut Vec<DownloadProgress>, new: DownloadProgress) {
    // 使用二分查找找到第一个起始点 >= new.start 的位置
    let i = progress.partition_point(|old| old.start < new.start);

    match i {
        0 => {
            // 处理开头位置的合并
            if !progress.is_empty() {
                let first = &mut progress[0];
                if new.end >= first.start {
                    if new.end > first.end {
                        first.end = new.end;
                    }
                    first.start = new.start.min(first.start);
                } else {
                    progress.insert(0, new);
                }
            } else {
                progress.push(new);
            }
        }
        _ => {
            let (left, right) = progress.split_at_mut(i);
            let prev = &mut left[i - 1];

            if prev.end >= new.start {
                // 与前一个区间重叠，合并
                prev.end = prev.end.max(new.end);
                // 检查是否需要与后续区间合并
                if !right.is_empty() && prev.end >= right[0].start {
                    let next = &right[0];
                    prev.end = prev.end.max(next.end);
                    progress.remove(i);
                }
            } else if !right.is_empty() && new.end >= right[0].start {
                // 只与后一个区间重叠
                right[0].start = new.start.min(right[0].start);
                right[0].end = right[0].end.max(new.end);
            } else {
                // 插入新区间
                progress.insert(i, new);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_progress() {
        let mut progresses = vec![DownloadProgress { start: 50, end: 60 }];

        // 调用独立的 merge_progress 函数
        merge_progress(&mut progresses, DownloadProgress { start: 10, end: 20 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 10, end: 20 },
                DownloadProgress { start: 50, end: 60 }
            ]
        );

        merge_progress(&mut progresses, DownloadProgress { start: 30, end: 40 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 10, end: 20 },
                DownloadProgress { start: 30, end: 40 },
                DownloadProgress { start: 50, end: 60 }
            ]
        );

        merge_progress(&mut progresses, DownloadProgress { start: 40, end: 50 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 10, end: 20 },
                DownloadProgress { start: 30, end: 60 },
            ]
        );

        merge_progress(&mut progresses, DownloadProgress { start: 60, end: 70 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 10, end: 20 },
                DownloadProgress { start: 30, end: 70 },
            ]
        );

        merge_progress(&mut progresses, DownloadProgress { start: 80, end: 90 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 10, end: 20 },
                DownloadProgress { start: 30, end: 70 },
                DownloadProgress { start: 80, end: 90 }
            ]
        );

        merge_progress(&mut progresses, DownloadProgress { start: 70, end: 80 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 10, end: 20 },
                DownloadProgress { start: 30, end: 90 },
            ]
        );

        merge_progress(&mut progresses, DownloadProgress { start: 0, end: 10 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 0, end: 20 },
                DownloadProgress { start: 30, end: 90 },
            ]
        );

        merge_progress(&mut progresses, DownloadProgress { start: 20, end: 30 });
        assert_eq!(progresses, vec![DownloadProgress { start: 0, end: 90 }]);
    }
}
