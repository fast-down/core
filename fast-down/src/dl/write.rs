use bytes::Bytes;
use color_eyre::Result;

use crate::Progress;

pub trait DownloadWriter: Send + Sync + Clone {
    fn write_part(&self, progress: Progress, bytes: Bytes) -> Result<()>;
    unsafe fn write_part_unchecked(&self, progress: Progress, bytes: Bytes) -> Result<()>;
}
