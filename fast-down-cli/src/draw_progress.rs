use crate::{fmt_size::format_file_size, fmt_time::format_time};
use fast_down::Progress;
use std::time::Instant;

pub fn draw_progress(
    start: Instant,
    total: usize,
    get_progress: &Vec<Progress>,
    last_get_size: usize,
    last_get_time: Instant,
    progress_width: usize,
    get_size: usize,
    avg_get_speed: &mut f64,
) {
    // 创建合并的进度条
    const BLOCK_CHARS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

    // 计算瞬时速度
    let get_elapsed_ms = last_get_time.elapsed().as_millis();
    let get_speed = if get_elapsed_ms > 0 {
        (get_size - last_get_size) as f64 * 1e3 / get_elapsed_ms as f64
    } else {
        0.0
    };

    // 更新下载速度队列
    const ALPHA: f64 = 0.9;
    *avg_get_speed = *avg_get_speed * ALPHA + get_speed * (1.0 - ALPHA);

    if total == 0 {
        // 处理空文件的情况，例如直接打印完成信息或一个特殊的进度条
        eprint!(
            "\x1b[1A\x1b[1A\r\x1B[K|{}| {:>6.2}% ({:>8}/{})\n\x1B[K已用时间: {} | 速度: {:>8}/s | 剩余: {}\n",
            BLOCK_CHARS[BLOCK_CHARS.len() - 1]
                .to_string()
                .repeat(progress_width), // 全满进度条
            100.0,
            format_file_size(get_size as f64),
            format_file_size(0.0),
            format_time(start.elapsed().as_secs()),
            format_file_size(*avg_get_speed),
            format_time(0)
        );
        return; // 不需要复杂的计算
    }

    // 计算百分比
    let get_percent = (get_size as f64 / total as f64) * 1e2;

    // 计算已用时间
    let elapsed = start.elapsed();

    // 下载剩余时间
    let get_remaining = if *avg_get_speed > 0.0 {
        (total as f64 - get_size as f64) / *avg_get_speed
    } else {
        0.0
    };

    // 格式化文件大小
    let formatted_get_size = format_file_size(get_size as f64);
    let formatted_total_size = format_file_size(total as f64);
    let formatted_get_speed = format_file_size(*avg_get_speed);
    let formatted_get_remaining = format_time(get_remaining as u64);
    let formatted_elapsed = format_time(elapsed.as_secs());

    // 构建进度条字符串
    let mut bar_str = String::with_capacity(progress_width);

    // 计算每个位置对应的字节范围
    let bytes_per_position = total as f64 / progress_width as f64;

    let mut current_progress_index = 0; // 指向get_progress的索引

    // 为进度条每个位置循环
    for pos in 0..progress_width {
        // 计算该位置对应的字节范围
        // 使用saturating_mul和saturating_add可防止在接近usize::MAX时溢出
        // 但目前保持更接近原始f64逻辑。f64仍可能存在精度问题
        let pos_start = (pos as f64 * bytes_per_position).floor() as usize;
        let pos_end = ((pos + 1) as f64 * bytes_per_position).floor() as usize;

        // 计算该位置代表的总字节数
        let position_total = pos_end.saturating_sub(pos_start);

        // 优化：如果该位置代表零字节（如因舍入或文件过小）
        // 则必为空
        if position_total == 0 {
            bar_str.push(BLOCK_CHARS[0]);
            continue;
        }

        // 推进进度索引越过在当前位置开始前就结束的块
        // 这些块对当前及后续位置都不会有影响
        while current_progress_index < get_progress.len()
            && get_progress[current_progress_index].end <= pos_start
        {
            current_progress_index += 1;
        }

        let mut get_completed_bytes = 0;

        // 从当前索引开始遍历可能相关的进度块
        for i in current_progress_index..get_progress.len() {
            let p = &get_progress[i];

            // 如果进度块开始位置在当前位置结束之后
            // 由于排序，该块及后续块都不可能重叠。可以停止搜索
            if p.start >= pos_end {
                break;
            }

            // 现在知道该块*可能*重叠(p.start < pos_end)
            // 且由于上述while循环，p.end > pos_start
            // 因此计算重叠部分
            let overlap_start = p.start.max(pos_start);
            let overlap_end = p.end.min(pos_end);

            // 确保overlap_end严格大于overlap_start才累加
            // 处理边界接触但内容不重叠的情况
            if overlap_end > overlap_start {
                get_completed_bytes += overlap_end - overlap_start;
            }
        }

        // 计算该位置的填充级别(0-8)
        let fill_level = if get_completed_bytes > 0 {
            // 使用浮点数计算比例
            let get_ratio = get_completed_bytes as f64 / position_total as f64;
            // 缩放至0-8，四舍五入，转为usize并安全截断
            ((get_ratio * 8.0).round() as usize).min(BLOCK_CHARS.len() - 1)
        } else {
            0 // 该段无完成字节
        };

        bar_str.push(BLOCK_CHARS[fill_level]);
    }

    eprint!(
        "\x1b[1A\x1b[1A\r\x1B[K|{}| {:>6.2}% ({:>8}/{})\n\x1B[K已用时间: {} | 速度: {:>8}/s | 剩余: {}\n",
        bar_str,
        get_percent,
        formatted_get_size,
        formatted_total_size,
        formatted_elapsed,
        formatted_get_speed,
        formatted_get_remaining
    );
}
