use crate::download_progress::DownloadProgress;
use memmap2::MmapOptions;
use rayon::prelude::*;
use std::{fs::File, io::Error};

const CHUNK_SIZE: usize = 1024 * 8; // 可调整块大小以优化性能

pub fn scan_file(file: &File) -> Result<Vec<DownloadProgress>, Error> {
    // 使用内存映射打开文件
    let mmap = unsafe { MmapOptions::new().map(file)? };

    // 并行处理每个块
    let ranges: Vec<_> = mmap
        .par_chunks(CHUNK_SIZE)
        .enumerate()
        .map(|(chunk_idx, chunk)| {
            let mut pos = chunk_idx * CHUNK_SIZE;
            let mut ranges = Vec::new();
            let mut current_start: Option<usize> = None;
            // 扫描当前块中的零字节
            for &byte in chunk.iter() {
                if byte == 0 {
                    if current_start.is_none() {
                        current_start = Some(pos);
                    }
                } else if let Some(start) = current_start.take() {
                    ranges.push(DownloadProgress::new(start, pos - 1));
                }
                pos += 1;
            }
            // 处理块末尾的未闭合区间
            if let Some(start) = current_start {
                ranges.push(DownloadProgress::new(start, pos - 1));
            }
            ranges
        })
        .filter(|r| !r.is_empty())
        .collect();
    let merged = merge_ranges(ranges);
    Ok(merged)
}

// 合并相邻的区间
fn merge_ranges(ranges: Vec<Vec<DownloadProgress>>) -> Vec<DownloadProgress> {
    if ranges.is_empty() {
        return vec![];
    }
    ranges
        .iter()
        .skip(1)
        .fold(ranges[0].clone(), |mut acc, range_group| {
            match acc.last_mut() {
                Some(last) if last.end + 1 == range_group[0].start => {
                    last.end = range_group[0].end;
                    acc.extend(range_group.iter().skip(1).cloned());
                }
                _ => acc.extend(range_group.iter().cloned()),
            }
            acc
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_file() -> Result<(), Box<dyn std::error::Error>> {
        let file = File::open(
            r"C:\Users\28546\Documents\code\fast-down-rust\downloads\debian-live-12.9.0-amd64-kde.iso",
        )?;
        let ranges = scan_file(&file)?;
        assert_eq!(ranges.len(), 16343669);
        Ok(())
    }
}
