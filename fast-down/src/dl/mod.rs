mod block_lock;
#[cfg(feature = "file")]
pub mod file_writer;
pub mod multi;
mod read_response;
pub mod single;
pub mod write;

#[cfg(feature = "file")]
pub mod download_file {
    extern crate alloc;
    extern crate std;

    use super::{multi, single};
    use crate::dl::file_writer::FileWriter;
    use crate::Event;
    use alloc::string::String;
    use alloc::vec::Vec;
    use color_eyre::eyre::Result;
    use core::{ops::Range, time::Duration};
    use reqwest::blocking::Client;
    use std::{fs, fs::OpenOptions, io::ErrorKind, path::Path, thread::JoinHandle};

    pub struct DownloadOptions<'a> {
        pub url: String,
        pub save_path: &'a Path,
        pub threads: usize,
        pub client: Client,
        pub file_size: usize,
        pub can_fast_download: bool,
        pub get_chunk_size: usize,
        pub download_chunks: Vec<Range<usize>>,
        pub retry_gap: Duration,
    }

    pub enum DownloadHandler {
        Single(JoinHandle<()>),
        Multi(Vec<JoinHandle<()>>),
    }

    impl DownloadHandler {
        pub fn join(self) {
            match self {
                DownloadHandler::Single(handle) => handle.join().unwrap(),
                DownloadHandler::Multi(handles) => {
                    for handle in handles {
                        handle.join().unwrap();
                    }
                }
            }
        }
    }

    pub struct DownloadResult {
        pub event_chan: crossbeam_channel::Receiver<Event>,
        pub handler: Option<DownloadHandler>,
    }

    impl DownloadResult {
        pub(crate) fn from_single(
            value: (crossbeam_channel::Receiver<Event>, JoinHandle<()>),
        ) -> Self {
            let (chan, handle) = value;
            Self {
                event_chan: chan,
                handler: Some(DownloadHandler::Single(handle)),
            }
        }

        pub(crate) fn from_multi(
            value: (crossbeam_channel::Receiver<Event>, Vec<JoinHandle<()>>),
        ) -> Self {
            let (chan, handles) = value;
            Self {
                event_chan: chan,
                handler: Some(DownloadHandler::Multi(handles)),
            }
        }

        pub fn join(&mut self) {
            self.handler.take().unwrap().join();
        }
    }

    pub fn download(options: DownloadOptions) -> Result<DownloadResult> {
        let save_folder = options.save_path.parent().unwrap();
        if let Err(e) = fs::create_dir_all(save_folder) {
            if e.kind() != ErrorKind::AlreadyExists {
                return Err(e.into());
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&options.save_path)?;
        file.set_len(options.file_size as u64)?;

        if !options.can_fast_download
            || options.threads < 2
                && options.download_chunks.len() == 1
                && options.download_chunks[0].start == 0
                && options.download_chunks[0].end == options.file_size
        {
            Ok(DownloadResult::from_single(single::download(
                options.url,
                FileWriter::new::<4>(file)?,
                single::DownloadOptions {
                    client: options.client,
                    get_chunk_size: options.get_chunk_size,
                    retry_gap: options.retry_gap,
                },
            )?))
        } else {
            Ok(DownloadResult::from_multi(multi::download(
                options.url,
                FileWriter::new::<4>(file)?,
                multi::DownloadOptions {
                    client: options.client,
                    threads: options.threads,
                    get_chunk_size: options.get_chunk_size,
                    download_chunks: options.download_chunks,
                    retry_gap: options.retry_gap,
                },
            )?))
        }
    }
}
