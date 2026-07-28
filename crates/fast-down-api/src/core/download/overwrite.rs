use crate::{
    DownloadState, Event, PartialConfig, Tx, core::download::pipeline::build_pipeline, tx_err,
};
use fast_down::{
    AnyError, DownloadResult, UrlInfo,
    fast_puller::FastDownPuller,
    http::HttpError,
    invert,
    multi::{TokioExecutor, download_multi},
    reqwest::SmartRedirectClient,
    single::download_single,
};
use inherit_config::ConfigLayer;
use path_helper::tokio::gen_unique_path;
use reqwest::Response;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::fs;
use tokio_util::sync::CancellationToken;

pub struct OverwriteOption {
    pub state: DownloadState,
    pub final_path: PathBuf,
    pub info: UrlInfo,
    pub resp: Response,
    pub tx: Tx,
    pub token: CancellationToken,
}

#[allow(clippy::too_many_lines)]
pub async fn overwrite(option: OverwriteOption) {
    let OverwriteOption {
        mut state,
        final_path,
        info,
        resp,
        tx,
        token,
    } = option;
    tx_err!(state.store().await, tx, StateSaveError);

    let inner = state.clone().build();
    let tmp_path = state.tmp_path();
    let url = &inner.url;
    let config = &inner.config;

    let pipeline = build_pipeline(url, config, &info, resp, &tmp_path, &tx, &token).await;
    let Some((puller, pusher)) = pipeline else {
        return;
    };

    let _ = tx.send(Event::Start {
        tmp_path: tmp_path.clone(),
        config_path: state.config_path.clone(),
        parsed_config: state.config.clone().unwrap_or_default(),
    });

    let res = if info.fast_download {
        download_multi(
            puller,
            pusher,
            fast_down::multi::DownloadOptions {
                download_chunks: invert(
                    config.downloaded_chunk.iter().cloned(),
                    info.size,
                    config.chunk_window,
                ),
                concurrent: config.threads,
                retry_gap: config.retry_gap,
                pull_timeout: config.pull_timeout,
                push_queue_cap: config.write_queue_cap,
                min_chunk_size: config.min_chunk_size,
                max_speculative: config.max_speculative,
            },
        )
    } else {
        download_single(
            puller,
            pusher,
            fast_down::single::DownloadOptions {
                retry_gap: config.retry_gap,
                push_queue_cap: config.write_queue_cap,
            },
        )
    };
    let _guard = abort_ctrl(&token, &res);

    let mut store_time = Instant::now();
    while let Ok(e) = res.event_chain().recv().await {
        if let fast_down::Event::PushProgress(range) = &e {
            state.merge_progress(range.clone());
        }
        let _ = match e {
            fast_down::Event::Pulling(id) => tx.send(Event::Pulling(id)),
            fast_down::Event::PullError(id, e) => tx.send(Event::PullError(id, anyhow::anyhow!(e))),
            fast_down::Event::PullTimeout(id) => tx.send(Event::PullTimeout(id)),
            fast_down::Event::PullProgress(id, range) => tx.send(Event::PullProgress(id, range)),
            fast_down::Event::Pushing(id, range) => tx.send(Event::Pushing(id, range)),
            fast_down::Event::PushError(id, range, e) => {
                tx.send(Event::PushError(id, range, anyhow::anyhow!(e)))
            }
            fast_down::Event::PushProgress(range) => tx.send(Event::PushProgress(range)),
            fast_down::Event::Flushing => tx.send(Event::Flushing),
            fast_down::Event::FlushError(e) => tx.send(Event::FlushError(anyhow::anyhow!(e))),
            fast_down::Event::Finished(id) => tx.send(Event::Finished(id)),
        };
        if store_time.elapsed() > Duration::from_secs(1) {
            if let Err(e) = state.store().await {
                let _ = tx.send(Event::StateSaveError(e));
            }
            store_time = Instant::now();
        }
    }

    if let Err(e) = res.join().await {
        let _ = tx.send(Event::JoinError(e));
    }
    let download_complete = info.size == 0
        || matches!(&state.config, Some(PartialConfig { downloaded_chunk: Some(x), .. }) if x.len() == 1 && x[0] == (0..info.size));
    if token.is_cancelled() || !download_complete {
        if let Err(e) = state.store().await {
            let _ = tx.send(Event::StateSaveError(e));
        }
        return;
    }

    let final_path = if config.overwrite {
        final_path
    } else {
        tx_err!(gen_unique_path(final_path).await, tx, GenPathError)
    };
    if let Err(e) = fs::rename(tmp_path, &final_path).await {
        if !config.overwrite {
            let _ = fs::remove_file(&final_path).await;
        }
        let _ = tx.send(Event::RenameFailed(e));
        return;
    }
    let _ = fs::remove_file(&state.config_path).await;
    let _ = tx.send(Event::Renamed(final_path));
}

struct Guard(tokio::task::JoinHandle<()>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn abort_ctrl(token: &CancellationToken, res: &MyDownloadResult) -> Guard {
    let token = token.clone();
    let res = res.clone();
    let handle = tokio::spawn(async move {
        token.cancelled().await;
        res.abort();
    });
    Guard(handle)
}

type MyDownloadResult = DownloadResult<
    TokioExecutor<FastDownPuller, Box<dyn AnyError + 'static>>,
    HttpError<SmartRedirectClient>,
    Box<dyn AnyError + 'static>,
>;
