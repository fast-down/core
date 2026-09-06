#![allow(clippy::unwrap_used)]
use super::*;
use fast_down::{FileId, ProgressEntry, UrlInfo};
use std::time::Duration;
use url::Url;

// Test-only helpers for the planning entry points. The `plan` / `plan_resume`
// / `plan_from_fd` paths all call `prefetch`, so the server-backed tests spin
// up a minimal local HTTP server that answers a normal GET with 200 +
// content-length and a Range probe with 206 + content-range (no identity
// headers, so `info.file_id` is `{None, None}` and a hand-written `.fd`
// validates against it as long as the size matches).
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;

fn make_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("plan_test_{name}_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn info_no_id(size: u64) -> UrlInfo {
    UrlInfo {
        size,
        raw_name: "a.bin".to_string(),
        supports_range: true,
        fast_download: true,
        final_url: Url::parse("https://example.com/a.bin").unwrap(),
        file_id: FileId::new(None, None),
        content_type: Some("application/octet-stream".to_string()),
    }
}

#[allow(clippy::too_many_lines)]
async fn spawn_server(body: &[u8]) -> Url {
    let body = body.to_vec();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let body = body.clone();
            let io = TokioIo::new(stream);
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let body = body.clone();
                    async move {
                        let total = body.len();
                        let is_range = req.headers().contains_key(RANGE);
                        let (status, content_length, body_bytes, content_range) = if is_range {
                            (
                                StatusCode::PARTIAL_CONTENT,
                                1usize,
                                Bytes::from(vec![0xABu8]),
                                Some(format!("bytes 0-0/{total}")),
                            )
                        } else {
                            (StatusCode::OK, total, Bytes::from(body), None)
                        };
                        let mut builder = Response::builder()
                            .status(status)
                            .header(CONTENT_LENGTH, content_length.to_string())
                            .header(ACCEPT_RANGES, "bytes");
                        if let Some(cr) = content_range {
                            builder = builder.header(CONTENT_RANGE, cr);
                        }
                        Ok::<_, Infallible>(builder.body(Full::new(body_bytes)).unwrap())
                    }
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    Url::parse(&format!("http://{addr}/file.bin")).unwrap()
}

async fn prefetch_info(url: &Url, tx: &Tx) -> UrlInfo {
    let cfg = Config::default();
    let (info, _resp) = prefetch(url, &cfg, tx).await.unwrap();
    info
}

// Write a `.fd` whose recorded size is `size` and whose identity fields are
// left at their serde defaults (matching a server that sends no
// etag/last-modified, so `info.file_id` is `{None, None}`). TOML has no
// `null` literal, so the identity fields are simply omitted.
async fn write_fd_raw(cfg_path: &Path, url: &Url, size: u64) {
    let toml = format!("url = \"{url}\"\nsize = {size}\n");
    tokio::fs::write(cfg_path, toml).await.unwrap();
}

// Build a `.fd` that validates against `info` (size + null identity) and,
// when `progress` is given, records that progress so a resume reports it.
async fn write_fd_matching(
    url: &Url,
    info: &UrlInfo,
    cfg_path: &Path,
    progress: Vec<ProgressEntry>,
) {
    let state = DownloadState::new(url, info, &PartialConfig::default(), cfg_path);
    for r in progress {
        state.merge_progress(r);
    }
    state.store().await.unwrap();
}

fn base_pc(save_dir: &Path, overwrite: bool) -> PartialConfig {
    PartialConfig {
        save_dir: Some(save_dir.to_path_buf()),
        filename: Some("a.bin".to_string()),
        overwrite: Some(overwrite),
        resume: Some(true),
        ..Default::default()
    }
}

fn make_info() -> UrlInfo {
    UrlInfo {
        size: 100,
        raw_name: "a.bin".to_string(),
        supports_range: true,
        fast_download: true,
        final_url: Url::parse("https://example.com/a.bin").unwrap(),
        file_id: FileId::new(Some("etag-1"), None),
        content_type: Some("application/octet-stream".to_string()),
    }
}

#[tokio::test]
async fn peek_resume_reads_saved_state_without_network() {
    let dir = std::env::temp_dir().join(format!(
        "fd_peek_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let part = dir.join("a.part");
    let fd = dir.join("a.fd");
    std::fs::write(&part, vec![0u8; 100]).unwrap();

    let url = Url::parse("https://example.com/a.bin").unwrap();
    let info = make_info();
    let state = DownloadState::new(&url, &info, &PartialConfig::default(), &fd);
    state.merge_progress(0u64..50);
    state.store().await.unwrap();

    let loaded = peek_resume(&part).await.unwrap();
    assert_eq!(loaded.get_progress(), vec![0u64..50]);
    assert_eq!(loaded.file_id(), FileId::new(Some("etag-1"), None));
    assert_eq!(
        loaded.lock_inner().url.as_ref().unwrap().as_str(),
        "https://example.com/a.bin"
    );
    assert_eq!(loaded.get_elapsed(), Duration::ZERO);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn peek_resume_rejects_non_part_path() {
    let dir = std::env::temp_dir().join(format!(
        "fd_peek_bad_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let not_part = dir.join("a.txt");
    std::fs::write(&not_part, b"x").unwrap();
    assert!(matches!(
        peek_resume(&not_part).await,
        Err(StateError::NotAPartFile(_))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Pure (no network) tests ----

#[tokio::test]
async fn resumable_outcome_sums_progress_lengths() {
    let dir = make_dir("res_outcome");
    let fd = dir.join("a.fd");
    let url = Url::parse("https://example.com/a.bin").unwrap();
    let info = info_no_id(1024);
    let state = DownloadState::new(&url, &info, &PartialConfig::default(), &fd);
    // Disjoint ranges so normalization keeps them separate.
    state.merge_progress(0u64..10);
    state.merge_progress(20u64..30);
    state.merge_progress(50u64..70);
    match resumable_outcome(&state, &fd) {
        ResumeOutcome::Resumable {
            downloaded,
            progress,
            ..
        } => {
            assert_eq!(downloaded, 40);
            assert_eq!(progress, vec![0u64..10, 20u64..30, 50u64..70]);
        }
        other => panic!("expected Resumable, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn state_url_prefers_caller_url_and_filters_non_http() {
    let url = Url::parse("https://example.com/a.bin").unwrap();
    let info = info_no_id(1024);
    let fd = PathBuf::from("dummy.fd");
    let loaded = DownloadState::new(&url, &info, &PartialConfig::default(), &fd);
    // Caller URL always wins.
    assert_eq!(state_url(Some(url.clone()), &loaded), Some(url.clone()));
    // No caller URL and a loaded http(s) URL -> resolved from the `.fd`.
    assert_eq!(state_url(None, &loaded), Some(url));
    // Loaded non-http(s) URL is filtered out -> None.
    loaded.update(|inner| inner.url = Some(Url::parse("ftp://host/x").unwrap()));
    assert_eq!(state_url(None, &loaded), None);
    // The default `about:blank` URL is also filtered out -> None.
    loaded.update(|inner| inner.url = Some(Url::parse("about:blank").unwrap()));
    assert_eq!(state_url(None, &loaded), None);
}

#[tokio::test]
async fn probe_resume_maps_outcome_without_network() {
    let dir = make_dir("probe_resume");
    let url = Url::parse("https://example.com/a.bin").unwrap();
    let info = info_no_id(1024);

    // Ok(Some): a valid `.fd` + `.part` pair validates and loads.
    let cfg = dir.join("a.fd");
    let tmp = dir.join("a.part");
    write_fd_raw(&cfg, &url, 1024).await;
    std::fs::write(&tmp, vec![0u8; 100]).unwrap();
    let (state, outcome) = probe_resume(&url, &cfg, &tmp, &info, &PartialConfig::default()).await;
    assert!(state.is_some());
    assert!(matches!(outcome, ResumeOutcome::Resumable { .. }));

    // Ok(None): `.fd` present but `.part` missing -> nothing usable.
    let cfg2 = dir.join("b.fd");
    let tmp2 = dir.join("b.part");
    write_fd_raw(&cfg2, &url, 1024).await;
    let (state, outcome) = probe_resume(&url, &cfg2, &tmp2, &info, &PartialConfig::default()).await;
    assert!(state.is_none());
    assert!(matches!(outcome, ResumeOutcome::Fresh));

    // Err(FileChanged): size recorded in the `.fd` no longer matches.
    let cfg3 = dir.join("c.fd");
    let tmp3 = dir.join("c.part");
    write_fd_raw(&cfg3, &url, 2048).await;
    std::fs::write(&tmp3, vec![0u8; 100]).unwrap();
    let (state, outcome) = probe_resume(&url, &cfg3, &tmp3, &info, &PartialConfig::default()).await;
    assert!(state.is_none());
    assert!(matches!(
        outcome,
        ResumeOutcome::Mismatch(StateError::FileChanged { .. })
    ));

    let _ = std::fs::remove_dir_all(&dir);
}

// A `.fd` that exists and is readable but is not valid TOML must surface as
// `Mismatch(StateError::Decode)`, not silently fall back to a fresh download.
// This is the load-stage error that `ResumeOutcome::Mismatch` can carry in
// addition to `FileChanged`; every existing mismatch test only exercises the
// `FileChanged` branch, so this path was previously uncovered.
#[tokio::test]
async fn probe_resume_maps_corrupt_fd_to_mismatch_decode() {
    let dir = make_dir("probe_resume_corrupt");
    let url = Url::parse("https://example.com/a.bin").unwrap();
    let info = info_no_id(1024);
    let cfg = dir.join("d.fd");
    let tmp = dir.join("d.part");
    std::fs::write(&cfg, b"not valid toml @@").unwrap();
    std::fs::write(&tmp, vec![0u8; 100]).unwrap();
    let (state, outcome) = probe_resume(&url, &cfg, &tmp, &info, &PartialConfig::default()).await;
    assert!(state.is_none());
    assert!(matches!(
        outcome,
        ResumeOutcome::Mismatch(StateError::Decode(..))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Server-backed tests for the planning entry points ----

#[tokio::test]
async fn plan_overwrite_true_no_pair_is_fresh() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("plan_ow_fresh");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let plan = plan(url, base_pc(&save_dir, true), tx, token)
        .await
        .unwrap();
    assert!(matches!(plan.resume_outcome(), ResumeOutcome::Fresh));
    assert!(plan.config().overwrite);
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_overwrite_true_resumable_pair_is_resumable() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("plan_ow_resume");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let info = prefetch_info(&url, &tx).await;
    let tmp = save_dir.join("a.bin.part");
    let cfg = save_dir.join("a.bin.fd");
    write_fd_matching(&url, &info, &cfg, vec![]).await;
    std::fs::write(&tmp, vec![0u8; 1024]).unwrap();
    let plan = plan(url, base_pc(&save_dir, true), tx, token)
        .await
        .unwrap();
    assert!(matches!(
        plan.resume_outcome(),
        ResumeOutcome::Resumable { .. }
    ));
    assert!(plan.tmp_path().ends_with("a.bin.part"));
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_overwrite_true_filechanged_is_mismatch() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("plan_ow_mismatch");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let tmp = save_dir.join("a.bin.part");
    let cfg = save_dir.join("a.bin.fd");
    write_fd_raw(&cfg, &url, 2048).await; // size mismatch vs 1024
    std::fs::write(&tmp, vec![0u8; 1024]).unwrap();
    let plan = plan(url, base_pc(&save_dir, true), tx, token)
        .await
        .unwrap();
    assert!(matches!(
        plan.resume_outcome(),
        ResumeOutcome::Mismatch(StateError::FileChanged { .. })
    ));
    let _ = std::fs::remove_dir_all(&save_dir);
}

// A corrupt `.fd` produces `Mismatch(StateError::Decode)`. Forcing the resume
// re-reads the same broken file and can only report the decode error and fail;
// `start_forced_resume` does not help for non-`FileChanged` mismatches, so the
// README hint to "use start_forced_resume" is a no-op for this case.
#[tokio::test]
async fn plan_overwrite_true_corrupt_fd_forced_resume_fails() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("plan_ow_corrupt_fd");
    let (tx, rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let tmp = save_dir.join("a.bin.part");
    let cfg = save_dir.join("a.bin.fd");
    std::fs::write(&cfg, b"not valid toml @@").unwrap();
    std::fs::write(&tmp, vec![0u8; 1024]).unwrap();
    let plan = plan(url, base_pc(&save_dir, true), tx, token)
        .await
        .unwrap();
    assert!(matches!(
        plan.resume_outcome(),
        ResumeOutcome::Mismatch(StateError::Decode(..))
    ));

    Box::pin(plan.start_forced_resume()).await;
    let mut reported_decode = false;
    let mut terminated = false;
    while let Ok(event) = rx.recv().await {
        reported_decode |= matches!(&event, Event::ResumeError(StateError::Decode(..)));
        if matches!(&event, Event::Terminated(_)) {
            terminated = true;
            break;
        }
    }
    assert!(
        reported_decode,
        "forced resume must surface the Decode error"
    );
    assert!(terminated, "forced resume must terminate, not hang");
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_no_overwrite_resumable_stem0() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("plan_nm_resume");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let info = prefetch_info(&url, &tx).await;
    let tmp = save_dir.join("a.bin.part");
    let cfg = save_dir.join("a.bin.fd");
    write_fd_matching(&url, &info, &cfg, vec![]).await;
    std::fs::write(&tmp, vec![0u8; 1024]).unwrap();
    let plan = plan(url, base_pc(&save_dir, false), tx, token)
        .await
        .unwrap();
    assert!(matches!(
        plan.resume_outcome(),
        ResumeOutcome::Resumable { .. }
    ));
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_no_overwrite_free_stem_is_fresh() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("plan_nm_fresh");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let plan = plan(url, base_pc(&save_dir, false), tx, token)
        .await
        .unwrap();
    assert!(matches!(plan.resume_outcome(), ResumeOutcome::Fresh));
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_no_overwrite_preserves_an_fd_only_manifest() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("plan_fd_only_reserved");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let info = prefetch_info(&url, &tx).await;
    let fd = save_dir.join("a.bin.fd");
    write_fd_matching(&url, &info, &fd, vec![]).await;

    let plan = plan(url, base_pc(&save_dir, false), tx, token)
        .await
        .unwrap();
    assert!(plan.tmp_path().ends_with("a (1).bin.part"));
    assert!(fd.exists());
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_no_overwrite_resume_disabled_ignores_pair() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("plan_nm_noresume");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let info = prefetch_info(&url, &tx).await;
    let tmp = save_dir.join("a.bin.part");
    let cfg = save_dir.join("a.bin.fd");
    write_fd_matching(&url, &info, &cfg, vec![]).await;
    std::fs::write(&tmp, vec![0u8; 1024]).unwrap();
    // `resume = false` must suppress the resume attempt even with a valid pair.
    let pc = PartialConfig {
        save_dir: Some(save_dir.clone()),
        filename: Some("a.bin".to_string()),
        overwrite: Some(false),
        resume: Some(false),
        ..Default::default()
    };
    let plan = plan(url, pc, tx, token).await.unwrap();
    assert!(matches!(plan.resume_outcome(), ResumeOutcome::Fresh));
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_resume_rejects_non_part_path() {
    let save_dir = make_dir("plan_res_badpath");
    let bad = save_dir.join("a.txt");
    std::fs::write(&bad, b"x").unwrap();
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let res = plan_resume(
        &bad,
        Some(Url::parse("https://example.com/a.bin").unwrap()),
        PartialConfig::default(),
        tx,
        token,
    )
    .await;
    assert!(matches!(
        res,
        Err(PlanError::Resume(StateError::Open(e))) if e.kind() == std::io::ErrorKind::InvalidInput
    ));
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_resume_missing_tmp_with_url_defers_to_fresh() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("plan_res_missing");
    let part = save_dir.join("a.bin.part"); // does NOT exist
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let plan = plan_resume(&part, Some(url), base_pc(&save_dir, false), tx, token)
        .await
        .unwrap();
    assert!(matches!(plan.resume_outcome(), ResumeOutcome::Fresh));
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_resume_missing_tmp_without_url_is_nourl() {
    let save_dir = make_dir("plan_res_missing_nourl");
    let part = save_dir.join("a.bin.part");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let res = plan_resume(&part, None, PartialConfig::default(), tx, token).await;
    assert!(matches!(res, Err(PlanError::Resume(StateError::NoUrl(_)))));
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_resume_valid_pair_is_resumable() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("plan_res_valid");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let info = prefetch_info(&url, &tx).await;
    let tmp = save_dir.join("a.bin.part");
    let cfg = save_dir.join("a.bin.fd");
    write_fd_matching(&url, &info, &cfg, vec![]).await;
    std::fs::write(&tmp, vec![0u8; 1024]).unwrap();
    let plan = plan_resume(&tmp, Some(url), base_pc(&save_dir, false), tx, token)
        .await
        .unwrap();
    assert!(matches!(
        plan.resume_outcome(),
        ResumeOutcome::Resumable { .. }
    ));
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_resume_filechanged_pair_is_mismatch() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("plan_res_fc");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let tmp = save_dir.join("a.bin.part");
    let cfg = save_dir.join("a.bin.fd");
    write_fd_raw(&cfg, &url, 2048).await;
    std::fs::write(&tmp, vec![0u8; 1024]).unwrap();
    let plan = plan_resume(&tmp, Some(url), base_pc(&save_dir, false), tx, token)
        .await
        .unwrap();
    assert!(matches!(
        plan.resume_outcome(),
        ResumeOutcome::Mismatch(StateError::FileChanged { .. })
    ));
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_from_fd_rejects_non_fd_path() {
    let save_dir = make_dir("fromfd_bad");
    let bad = save_dir.join("a.part");
    std::fs::write(&bad, b"x").unwrap();
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let res = plan_from_fd(&bad, None, PartialConfig::default(), tx, token).await;
    assert!(matches!(
        res,
        Err(PlanError::Resume(StateError::Open(e))) if e.kind() == std::io::ErrorKind::InvalidInput
    ));
    let _ = std::fs::remove_dir_all(&save_dir);
}

// A manifest with no usable `.part` still contributes its URL/config, but
// the bytes start at zero and must therefore be reported as Fresh.
#[tokio::test]
async fn plan_from_fd_fd_only_is_fresh() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("fromfd_only");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let info = prefetch_info(&url, &tx).await;
    let fd = save_dir.join("a.bin.fd");
    // No `.part` present at all.
    write_fd_matching(&url, &info, &fd, vec![]).await;
    let plan = plan_from_fd(&fd, Some(url), base_pc(&save_dir, false), tx, token)
        .await
        .unwrap();
    assert!(matches!(plan.resume_outcome(), ResumeOutcome::Fresh));
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
#[allow(clippy::single_range_in_vec_init)]
async fn plan_from_fd_with_part_resumes_recorded_progress() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("fromfd_part");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let info = prefetch_info(&url, &tx).await;
    let fd = save_dir.join("a.bin.fd");
    let tmp = save_dir.join("a.bin.part");
    write_fd_matching(&url, &info, &fd, vec![0u64..512]).await;
    std::fs::write(&tmp, vec![0u8; 600]).unwrap(); // >= recorded frontier 512
    let plan = plan_from_fd(&fd, Some(url), base_pc(&save_dir, false), tx, token)
        .await
        .unwrap();
    match plan.resume_outcome() {
        ResumeOutcome::Resumable { downloaded, .. } => assert_eq!(*downloaded, 512),
        other => panic!("expected Resumable, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_from_fd_filechanged_part_falls_back_to_fresh_manifest() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("fromfd_fc");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let tmp = save_dir.join("a.bin.part");
    let fd = save_dir.join("a.bin.fd");
    write_fd_raw(&fd, &url, 2048).await; // size mismatch vs 1024
    std::fs::write(&tmp, vec![0u8; 1024]).unwrap();
    let plan = plan_from_fd(&fd, Some(url), base_pc(&save_dir, false), tx, token)
        .await
        .unwrap();
    assert!(matches!(plan.resume_outcome(), ResumeOutcome::Fresh));
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_from_fd_resolves_url_from_fd() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("fromfd_resolve");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let info = prefetch_info(&url, &tx).await;
    let fd = save_dir.join("a.bin.fd");
    write_fd_matching(&url, &info, &fd, vec![]).await; // `.fd` carries the URL
    let plan = plan_from_fd(&fd, None, base_pc(&save_dir, false), tx, token)
        .await
        .unwrap();
    assert!(matches!(plan.resume_outcome(), ResumeOutcome::Fresh));
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_resume_inherits_persisted_config_before_probe() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("resume_inherit_config");
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let info = prefetch_info(&url, &tx).await;
    let final_path = save_dir.join("persisted.bin");
    let tmp = final_path.with_added_extension("part");
    let fd = final_path.with_added_extension("fd");
    let persisted = PartialConfig {
        save_dir: Some(save_dir.clone()),
        filename: Some("persisted.bin".to_string()),
        threads: Some(3),
        ..Default::default()
    };
    DownloadState::new(&url, &info, &persisted, &fd)
        .store()
        .await
        .unwrap();
    std::fs::write(&tmp, vec![0u8; 1024]).unwrap();

    let plan = plan_resume(&tmp, Some(url), PartialConfig::default(), tx, token)
        .await
        .unwrap();
    assert!(plan.final_path().ends_with("persisted.bin"));
    assert_eq!(plan.config().save_dir, save_dir);
    assert_eq!(plan.config().threads, 3);
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
#[allow(clippy::single_range_in_vec_init)]
async fn fresh_plan_discards_unvalidated_caller_progress() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("fresh_discards_progress");
    let (tx, rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let plan = plan(
        url,
        PartialConfig {
            save_dir: Some(save_dir.clone()),
            filename: Some("a.bin".to_string()),
            overwrite: Some(true),
            downloaded_chunk: Some(vec![0u64..512]),
            ..Default::default()
        },
        tx,
        token.clone(),
    )
    .await
    .unwrap();
    token.cancel();
    Box::pin(plan.start()).await;
    while let Ok(event) = rx.recv().await {
        if matches!(&event, Event::Terminated(_)) {
            break;
        }
    }
    let state = DownloadState::load(&save_dir.join("a.bin.fd"))
        .await
        .unwrap();
    assert_eq!(state.get_progress(), Vec::new());
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
#[allow(clippy::single_range_in_vec_init)]
async fn start_rechecks_a_resumable_part_that_disappeared() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("resume_part_disappeared");
    let (tx, rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let info = prefetch_info(&url, &tx).await;
    let tmp = save_dir.join("a.bin.part");
    let fd = save_dir.join("a.bin.fd");
    write_fd_matching(&url, &info, &fd, vec![0u64..512]).await;
    std::fs::write(&tmp, vec![0u8; 600]).unwrap();
    let plan = plan_resume(
        &tmp,
        Some(url),
        base_pc(&save_dir, false),
        tx,
        token.clone(),
    )
    .await
    .unwrap();
    std::fs::remove_file(&tmp).unwrap();
    token.cancel();
    Box::pin(plan.start()).await;

    let mut resumed = false;
    while let Ok(event) = rx.recv().await {
        resumed |= matches!(&event, Event::Resumed { .. });
        if matches!(&event, Event::Terminated(_)) {
            break;
        }
    }
    assert!(!resumed, "a missing .part must not be announced as resumed");
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn forced_resume_revalidates_the_reloaded_fd_size() {
    let body = vec![0xABu8; 1024];
    let url = spawn_server(&body).await;
    let save_dir = make_dir("forced_reloads_size");
    let (tx, rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let info = prefetch_info(&url, &tx).await;
    let tmp = save_dir.join("a.bin.part");
    let fd = save_dir.join("a.bin.fd");
    let mut stale_info = info.clone();
    stale_info.file_id = FileId::new(Some("old-etag"), None);
    DownloadState::new(&url, &stale_info, &PartialConfig::default(), &fd)
        .store()
        .await
        .unwrap();
    std::fs::write(&tmp, vec![0u8; 2048]).unwrap();
    let plan = plan_resume(
        &tmp,
        Some(url.clone()),
        base_pc(&save_dir, false),
        tx,
        token,
    )
    .await
    .unwrap();
    assert!(matches!(
        plan.resume_outcome(),
        ResumeOutcome::Mismatch(StateError::FileChanged {
            local_file_size: 1024,
            remote_file_size: 1024,
            ..
        })
    ));

    write_fd_raw(&fd, &url, 2048).await;
    Box::pin(plan.start_forced_resume()).await;
    let mut rejected_new_size = false;
    while let Ok(event) = rx.recv().await {
        rejected_new_size |= matches!(
            &event,
            Event::ResumeError(StateError::FileChanged {
                local_file_size: 2048,
                remote_file_size: 1024,
                ..
            })
        );
        if matches!(&event, Event::Terminated(_)) {
            break;
        }
    }
    assert!(rejected_new_size);
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn plan_from_fd_no_url_and_unresolvable_fd_is_nourl() {
    let save_dir = make_dir("fromfd_nourl");
    let fd = save_dir.join("a.bin.fd");
    write_fd_raw(&fd, &Url::parse("about:blank").unwrap(), 1024).await;
    let (tx, _rx) = crate::create_channel();
    let token = crate::create_cancellation_token();
    let res = plan_from_fd(&fd, None, PartialConfig::default(), tx, token).await;
    assert!(matches!(res, Err(PlanError::Resume(StateError::NoUrl(_)))));
    let _ = std::fs::remove_dir_all(&save_dir);
}
