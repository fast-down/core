use crate::{fmt_size::format_file_size, fmt_time::format_time};
use color_eyre::Result;
use crossterm::{
    cursor,
    style::Print,
    terminal::{self, ClearType},
    ExecutableCommand, QueueableCommand,
};
use fast_down::{MergeProgress, Progress, Total};
use std::{
    io::{self, Stderr, Stdout, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const BLOCK_CHARS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

pub struct ProgressPainter {
    pub progress: Vec<Progress>,
    pub total: u64,
    pub width: usize,
    pub start_time: Instant,
    pub alpha: f64,
    pub prev_size: u64,
    pub curr_size: u64,
    pub avg_speed: f64,
    pub repaint_duration: Duration,
    pub last_repaint_time: Instant,
    has_progress: bool,
    stderr: Stderr,
    stdout: Stdout,
}

impl ProgressPainter {
    pub fn new(
        init_progress: Vec<Progress>,
        total: u64,
        progress_width: usize,
        alpha: f64,
        repaint_duration: Duration,
    ) -> Self {
        debug_assert_ne!(progress_width, 0);
        let init_size = init_progress.total();
        Self {
            progress: init_progress,
            total,
            width: progress_width,
            alpha,
            repaint_duration,
            start_time: Instant::now(),
            prev_size: init_size,
            curr_size: init_size,
            avg_speed: 0.0,
            last_repaint_time: Instant::now(),
            has_progress: false,
            stdout: io::stdout(),
            stderr: io::stderr(),
        }
    }

    pub fn start_update_thread(painter_arc: Arc<Mutex<ProgressPainter>>) -> Box<impl Fn()> {
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        thread::spawn(move || loop {
            let mut painter = painter_arc.lock().unwrap();
            painter.update().unwrap();
            let should_stop = painter.total > 0 && painter.curr_size >= painter.total;
            let duration = painter.repaint_duration;
            drop(painter);
            if should_stop || !running.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(duration);
        });
        Box::new(move || {
            running_clone.store(false, Ordering::Relaxed);
        })
    }

    pub fn add(&mut self, p: Progress) {
        self.curr_size += p.total();
        self.progress.merge_progress(p);
    }

    pub fn update(&mut self) -> Result<()> {
        let repaint_elapsed = self.last_repaint_time.elapsed();
        self.last_repaint_time = Instant::now();
        let repaint_elapsed_ms = repaint_elapsed.as_millis();
        let curr_dsize = self.curr_size - self.prev_size;
        let get_speed = if repaint_elapsed_ms > 0 {
            (curr_dsize * 1000) as f64 / repaint_elapsed_ms as f64
        } else {
            0.0
        };
        self.avg_speed = self.avg_speed * self.alpha + get_speed * (1.0 - self.alpha);
        self.prev_size = self.curr_size;
        let progress_str = if self.total == 0 {
            format!(
                "|{}| {:>6.2}% ({:>8}/Unknown)\n已用时间: {} | 速度: {:>8}/s | 剩余: Unknown\n",
                BLOCK_CHARS[0].to_string().repeat(self.width),
                0.0,
                format_file_size(self.curr_size as f64),
                format_time(self.start_time.elapsed().as_secs()),
                format_file_size(self.avg_speed)
            )
        } else {
            let get_percent = (self.curr_size as f64 / self.total as f64) * 100.0;
            let get_remaining_time = (self.total - self.curr_size) as f64 / self.avg_speed;
            let per_bytes = self.total as f64 / self.width as f64;
            let mut bar_values = vec![0u64; self.width];
            let mut index = 0;
            for i in 0..self.width {
                let start_byte = i as f64 * per_bytes;
                let end_byte = (start_byte + per_bytes) as u64;
                let start_byte = start_byte as u64;
                let mut block_total = 0;
                for segment in &self.progress[index..] {
                    if segment.end <= start_byte {
                        index += 1;
                        continue;
                    }
                    if segment.start >= end_byte {
                        break;
                    }
                    let overlap_start = segment.start.max(start_byte);
                    let overlap_end = segment.end.min(end_byte);
                    if overlap_start < overlap_end {
                        block_total += overlap_end - overlap_start;
                    }
                }
                bar_values[i] = block_total;
            }
            let bar_str: String = bar_values
                .iter()
                .map(|&count| {
                    BLOCK_CHARS[((count as f64 / per_bytes as f64 * (BLOCK_CHARS.len() - 1) as f64)
                        .round() as usize)
                        .min(BLOCK_CHARS.len() - 1)]
                })
                .collect();
            format!(
                "|{}| {:>6.2}% ({:>8}/{})\n已用时间: {} | 速度: {:>8}/s | 剩余: {}\n",
                bar_str,
                get_percent,
                format_file_size(self.curr_size as f64),
                format_file_size(self.total as f64),
                format_time(self.start_time.elapsed().as_secs()),
                format_file_size(self.avg_speed),
                format_time(get_remaining_time as u64)
            )
        };
        if self.has_progress {
            self.stderr
                .queue(cursor::MoveUp(1))?
                .queue(terminal::Clear(ClearType::CurrentLine))?
                .queue(cursor::MoveUp(1))?
                .queue(terminal::Clear(ClearType::CurrentLine))?;
        }
        self.has_progress = true;
        self.stderr.queue(Print(progress_str))?;
        self.stderr.flush()?;
        Ok(())
    }

    pub fn print(&mut self, msg: &str) -> Result<()> {
        if self.has_progress {
            self.stderr
                .queue(cursor::MoveUp(1))?
                .queue(terminal::Clear(ClearType::CurrentLine))?
                .queue(cursor::MoveUp(1))?
                .queue(terminal::Clear(ClearType::CurrentLine))?;
        }
        self.stdout.execute(Print(msg))?;
        self.has_progress = false;
        self.update()?;
        Ok(())
    }
}
