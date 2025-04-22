#![feature(step_trait)]

pub mod dl;
mod event;
mod fmt;
mod merge_progress;
mod prefetch;
mod progress;
mod total;

#[cfg(feature = "file")]
pub use dl::download_file::{download, DownloadOptions, DownloadResult};
pub use event::Event;
pub use fmt::size::format_file_size;
pub use merge_progress::MergeProgress;
pub use prefetch::{get_url_info, UrlInfo};
pub use progress::Progress;
pub use total::Total;
