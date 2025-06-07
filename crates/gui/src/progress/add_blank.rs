use crate::home_page::ProgressData;
use fast_down::ProgressEntry;

pub fn add_blank(progress: &[ProgressEntry], total_size: u64) -> Vec<ProgressData> {
    if progress.is_empty() {
        return vec![ProgressData {
            is_blank: true,
            width: 1.0,
        }];
    }
    let mut result = Vec::with_capacity(2 * progress.len() + 1);
    let mut prev_end = 0;
    for range in progress {
        if range.start > prev_end {
            result.push(ProgressData {
                is_blank: true,
                width: (range.start - prev_end) as f32 / total_size as f32,
            });
        }
        prev_end = range.end;
        result.push(ProgressData {
            is_blank: false,
            width: (range.end - range.start) as f32 / total_size as f32,
        });
    }
    if prev_end < total_size {
        result.push(ProgressData {
            is_blank: true,
            width: (total_size - prev_end) as f32 / total_size as f32,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_blank() {
        assert_eq!(
            add_blank(&vec![], 10),
            vec![ProgressData {
                is_blank: true,
                width: 1.0,
            }]
        );
        assert_eq!(
            add_blank(&vec![0..5], 10),
            vec![
                ProgressData {
                    is_blank: false,
                    width: 0.5,
                },
                ProgressData {
                    is_blank: true,
                    width: 0.5,
                }
            ]
        );
        assert_eq!(
            add_blank(&vec![5..10], 10),
            vec![
                ProgressData {
                    is_blank: true,
                    width: 0.5,
                },
                ProgressData {
                    is_blank: false,
                    width: 0.5,
                }
            ]
        );
    }
}
