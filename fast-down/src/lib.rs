#![feature(step_trait)]

mod fmt;
pub mod dl;
mod event;
mod prefetch;
mod merge_progress;
mod progress;
mod total;

#[cfg(feature = "file")]
pub use dl::download_file::{download, DownloadOptions, DownloadResult};
pub use event::Event;
pub use fmt::size::format_file_size;
pub use prefetch::{get_url_info, UrlInfo};
pub use merge_progress::MergeProgress;
pub use progress::Progress;
pub use total::Total;
