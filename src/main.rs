use fast_down_rust::{download, DownloadOptions, DownloadProgress};
use reqwest::header::{HeaderMap, HeaderValue};

#[tokio::main]
async fn main() {
    let mut headers = HeaderMap::new();
    headers.insert("User-Agent", HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36"));

    let mut progress: Vec<DownloadProgress> = Vec::new();

    let mut r = download(DownloadOptions {
        url: "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/12.9.0-live/amd64/iso-hybrid/debian-live-12.9.0-amd64-kde.iso",
        threads: 32,
        save_folder: "./test/",
        file_name: None,
        headers: Some(headers),
        proxy: None,
    })
    .await
    .unwrap();
    println!("{:#?}", r);

    while let Some(e) = r.rx.recv().await {
        progress.merge_progress(e);
        draw_progress(r.file_size, &progress);
    }
}

fn draw_progress(total: usize, downloaded: &[DownloadProgress]) {
    print!("\r{}: {:?}", total, downloaded);
}

trait MergeProgress {
    fn merge_progress(&mut self, new: DownloadProgress);
}

impl MergeProgress for Vec<DownloadProgress> {
    fn merge_progress(&mut self, new: DownloadProgress) {
        // 使用二分查找找到第一个起始点 >= new.start 的位置
        let i = self.partition_point(|old| old.start < new.start);

        match i {
            0 => {
                // 处理开头位置的合并
                if !self.is_empty() {
                    let first = &mut self[0];
                    if new.end >= first.start {
                        if new.end > first.end {
                            first.end = new.end;
                        }
                        first.start = new.start.min(first.start);
                    } else {
                        self.insert(0, new);
                    }
                } else {
                    self.push(new);
                }
            }
            _ => {
                let (left, right) = self.split_at_mut(i);
                let prev = &mut left[i - 1];

                if prev.end >= new.start {
                    // 与前一个区间重叠，合并
                    prev.end = prev.end.max(new.end);
                    // 检查是否需要与后续区间合并
                    if !right.is_empty() && prev.end >= right[0].start {
                        let next = &right[0];
                        prev.end = prev.end.max(next.end);
                        self.remove(i);
                    }
                } else if !right.is_empty() && new.end >= right[0].start {
                    // 只与后一个区间重叠
                    right[0].start = new.start.min(right[0].start);
                    right[0].end = right[0].end.max(new.end);
                } else {
                    // 插入新区间
                    self.insert(i, new);
                }
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
        progresses.merge_progress(DownloadProgress { start: 10, end: 20 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 10, end: 20 },
                DownloadProgress { start: 50, end: 60 }
            ]
        );
        progresses.merge_progress(DownloadProgress { start: 30, end: 40 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 10, end: 20 },
                DownloadProgress { start: 30, end: 40 },
                DownloadProgress { start: 50, end: 60 }
            ]
        );
        progresses.merge_progress(DownloadProgress { start: 40, end: 50 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 10, end: 20 },
                DownloadProgress { start: 30, end: 60 },
            ]
        );
        progresses.merge_progress(DownloadProgress { start: 60, end: 70 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 10, end: 20 },
                DownloadProgress { start: 30, end: 70 },
            ]
        );
        progresses.merge_progress(DownloadProgress { start: 80, end: 90 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 10, end: 20 },
                DownloadProgress { start: 30, end: 70 },
                DownloadProgress { start: 80, end: 90 }
            ]
        );
        progresses.merge_progress(DownloadProgress { start: 70, end: 80 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 10, end: 20 },
                DownloadProgress { start: 30, end: 90 },
            ]
        );
        progresses.merge_progress(DownloadProgress { start: 0, end: 10 });
        assert_eq!(
            progresses,
            vec![
                DownloadProgress { start: 0, end: 20 },
                DownloadProgress { start: 30, end: 90 },
            ]
        );
        progresses.merge_progress(DownloadProgress { start: 20, end: 30 });
        assert_eq!(progresses, vec![DownloadProgress { start: 0, end: 90 },])
    }
}
