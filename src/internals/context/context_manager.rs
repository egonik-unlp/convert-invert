use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::Context;
use tokio::{
    sync::{
        Semaphore,
        mpsc::{self, Receiver, Sender},
    },
    time::sleep,
};

use crate::internals::{
    download::download_manager::DownloadManager,
    judge::judge_manager::JudgeManager,
    query::query_manager::QueryManager,
    search::search_manager::{JudgeSubmission, SearchItem, SearchManager, Status},
};

#[allow(dead_code)]
#[derive(Debug)]
pub struct ContextManager {
    status: mpsc::Receiver<Status>,
}

#[derive(Debug)]
pub struct DownloadedFile {
    pub submission: JudgeSubmission,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub item: SearchItem,
    pub search_attempts: u8,
    pub judge_attempts: u8,
    pub download_attempts: u8,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct SearchResults {
    pub request: SearchRequest,
    pub submissions: Vec<JudgeSubmission>,
}

#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub request: SearchRequest,
    pub submission: JudgeSubmission,
}

#[derive(Debug, Clone)]
pub struct JudgeResults {
    pub request: SearchRequest,
    pub accepted: Vec<DownloadRequest>,
    pub total: usize,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct RetryRequest {
    pub target: RetryTarget,
    pub reason: RetryReason,
    pub backoff_secs: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum DownloadFailure {
    Failed,
    TimedOut,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct DownloadFailureInfo {
    pub request: SearchRequest,
    pub submission: JudgeSubmission,
    pub failure: DownloadFailure,
}

#[derive(Debug, Clone)]
pub enum RetryTarget {
    Search(SearchRequest),
}

#[derive(Debug, Clone)]
pub enum RetryReason {
    SearchNoResults,
    JudgeNoMatch,
    DownloadFailed(DownloadFailure),
}

#[derive(Debug)]
pub enum Track {
    Query(Vec<SearchItem>),
    Search(SearchRequest),
    SearchResults(SearchResults),
    Judge(SearchResults),
    JudgeResults(JudgeResults),
    Downloadable(DownloadRequest),
    DownloadFailed(DownloadFailureInfo),
    File(DownloadedFile),
    Retry(RetryRequest),
}

pub trait Manager {
    fn run(self) -> anyhow::Result<()>;
}

#[allow(dead_code)]
pub struct Managers {
    pub download_manager: DownloadManager,
    pub search_manager: SearchManager,
    pub query_manager: QueryManager,
    pub judge_manager: JudgeManager,
    pub search_timeout_secs: u64,
    pub max_search_timeout_secs: u64,
    pub max_search_retries: u8,
    pub max_judge_retries: u8,
    pub max_download_retries: u8,
    pub base_backoff_secs: u64,
    pub max_backoff_secs: u64,
    pub max_concurrent_searches: usize,
    pub max_concurrent_judges: usize,
    pub max_concurrent_downloads: usize,
}

impl Managers {
    pub fn new(_score: i32) -> Self {
        // let config = config::config_manager::Config::try_from_env().unwrap();
        // let client = Arc::new(Client::new(config.user_name, config.user_password));
        // let download_manager = DownloadManager::new(client.clone(), "~/Music/falopa", download_queue);
        // let search_manager = SearchManager::new(client.clone(), );
        // let judge_manager = JudgeManager::new(judge_queue, download_queue, method):
        // let query_manager = QueryManager::new(, data_tx);
        // Managers {
        //     download_manager,
        //     search_manager,
        //     judge_manager,
        //     query_manager
        // }
        todo!();
    }
}

#[allow(unused_variables)]
pub async fn manager(
    mut receiver: Receiver<Track>,
    sender: Sender<Track>,
    managers: Managers,
    pending: Arc<AtomicUsize>,
) -> anyhow::Result<()> {
    let inflight = Arc::new(AtomicUsize::new(0));
    let search_sem = Arc::new(Semaphore::new(managers.max_concurrent_searches));
    let judge_sem = Arc::new(Semaphore::new(managers.max_concurrent_judges));
    let download_sem = Arc::new(Semaphore::new(managers.max_concurrent_downloads));
    while let Some(track) = receiver.recv().await {
        pending.fetch_sub(1, Ordering::SeqCst);
        match track {
            Track::Query(search_items) => {
                tracing::info!(count = search_items.len(), "query_dispatch");
                for search_item in search_items {
                    let request = SearchRequest {
                        item: search_item,
                        search_attempts: 0,
                        judge_attempts: 0,
                        download_attempts: 0,
                        timeout_secs: managers.search_timeout_secs,
                    };
                    send_track(&sender, Track::Search(request), &pending).await?;
                }
            }
            Track::Search(request) => {
                tracing::info!(
                    stage = "search",
                    id = request.item.id,
                    attempts = request.search_attempts,
                    timeout_secs = request.timeout_secs,
                    "start"
                );
                let sender = sender.clone();
                let pending = Arc::clone(&pending);
                let inflight = Arc::clone(&inflight);
                let search_sem = Arc::clone(&search_sem);
                let search_manager = managers.search_manager.context_clone();
                inflight.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let permit = search_sem.acquire_owned().await;
                    if permit.is_err() {
                        tracing::warn!("Search semaphore closed");
                        inflight.fetch_sub(1, Ordering::SeqCst);
                        return;
                    }
                    let results = search_manager.run(request).await;
                    match results {
                        Ok(res) => {
                            tracing::info!(
                                stage = "search",
                                id = res.request.item.id,
                                attempts = res.request.search_attempts,
                                results = res.submissions.len(),
                                "end"
                            );
                            let _ = send_track(&sender, Track::SearchResults(res), &pending).await;
                        }
                        Err(err) => {
                            tracing::error!(stage = "search", error = ?err, "failed");
                        }
                    }
                    inflight.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Track::SearchResults(results) => {
                if results.submissions.is_empty() {
                    tracing::info!(
                        stage = "search",
                        id = results.request.item.id,
                        attempts = results.request.search_attempts,
                        "no_results"
                    );
                    if let Some(retry) = build_retry(
                        &managers,
                        results.request.clone(),
                        RetryReason::SearchNoResults,
                    ) {
                        send_track(&sender, Track::Retry(retry), &pending).await?;
                    } else {
                        tracing::warn!(
                            item = ?results.request.item,
                            "Search retry limit reached"
                        );
                    }
                } else {
                    send_track(&sender, Track::Judge(results), &pending).await?;
                }
            }
            Track::Judge(results) => {
                tracing::info!(
                    stage = "judge",
                    id = results.request.item.id,
                    attempts = results.request.judge_attempts,
                    candidates = results.submissions.len(),
                    "start"
                );
                let sender = sender.clone();
                let pending = Arc::clone(&pending);
                let inflight = Arc::clone(&inflight);
                let judge_sem = Arc::clone(&judge_sem);
                let judge_manager = managers.judge_manager.context_clone();
                inflight.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let permit = judge_sem.acquire_owned().await;
                    if permit.is_err() {
                        tracing::warn!("Judge semaphore closed");
                        inflight.fetch_sub(1, Ordering::SeqCst);
                        return;
                    }
                    let judged = judge_manager.run(results).await;
                    match judged {
                        Ok(res) => {
                            tracing::info!(
                                stage = "judge",
                                id = res.request.item.id,
                                attempts = res.request.judge_attempts,
                                accepted = res.accepted.len(),
                                total = res.total,
                                "end"
                            );
                            let _ = send_track(&sender, Track::JudgeResults(res), &pending).await;
                        }
                        Err(err) => {
                            tracing::error!(stage = "judge", error = ?err, "failed");
                        }
                    }
                    inflight.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Track::JudgeResults(results) => {
                if results.accepted.is_empty() {
                    tracing::info!(
                        stage = "judge",
                        id = results.request.item.id,
                        attempts = results.request.judge_attempts,
                        total = results.total,
                        "no_match"
                    );
                    if let Some(retry) = build_retry(
                        &managers,
                        results.request.clone(),
                        RetryReason::JudgeNoMatch,
                    ) {
                        send_track(&sender, Track::Retry(retry), &pending).await?;
                    } else {
                        tracing::warn!(
                            item = ?results.request.item,
                            total = results.total,
                            "Judge retry limit reached"
                        );
                    }
                } else {
                    send_tracks(
                        &sender,
                        results
                            .accepted
                            .into_iter()
                            .map(Track::Downloadable)
                            .collect(),
                        &pending,
                    )
                    .await?;
                }
            }
            Track::Downloadable(download_request) => {
                tracing::info!(
                    stage = "download",
                    id = download_request.request.item.id,
                    attempts = download_request.request.download_attempts,
                    file = %download_request.submission.query.filename,
                    user = %download_request.submission.query.username,
                    "start"
                );
                let sender = sender.clone();
                let pending = Arc::clone(&pending);
                let inflight = Arc::clone(&inflight);
                let download_sem = Arc::clone(&download_sem);
                let download_manager = managers.download_manager.context_clone();
                inflight.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let permit = download_sem.acquire_owned().await;
                    if permit.is_err() {
                        tracing::warn!("Download semaphore closed");
                        inflight.fetch_sub(1, Ordering::SeqCst);
                        return;
                    }
                    let track = download_manager.run(download_request).await;
                    match track {
                        Ok(res) => {
                            let _ = send_track(&sender, res, &pending).await;
                        }
                        Err(err) => {
                            tracing::error!(stage = "download", error = ?err, "failed");
                        }
                    }
                    inflight.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Track::DownloadFailed(failure) => {
                tracing::warn!(
                    stage = "download",
                    id = failure.request.item.id,
                    attempts = failure.request.download_attempts,
                    failure = ?failure.failure,
                    "failed"
                );
                if let Some(retry) = build_retry(
                    &managers,
                    failure.request.clone(),
                    RetryReason::DownloadFailed(failure.failure),
                ) {
                    send_track(&sender, Track::Retry(retry), &pending).await?;
                } else {
                    tracing::warn!(failure = ?failure, "Download retry limit reached");
                }
            }
            Track::File(downloaded_file) => {
                tracing::info!(
                    file = ?downloaded_file.path,
                    query = ?downloaded_file.submission.query,
                    stage = "download",
                    "complete"
                );
            }
            Track::Retry(retry_request) => {
                tracing::warn!(
                    reason = ?retry_request.reason,
                    backoff_secs = retry_request.backoff_secs,
                    "retry_scheduled"
                );
                let sender = sender.clone();
                let pending = Arc::clone(&pending);
                let inflight = Arc::clone(&inflight);
                inflight.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    if retry_request.backoff_secs > 0 {
                        sleep(std::time::Duration::from_secs(retry_request.backoff_secs)).await;
                    }
                    match retry_request.target {
                        RetryTarget::Search(request) => {
                            tracing::info!(
                                stage = "search",
                                id = request.item.id,
                                attempts = request.search_attempts,
                                timeout_secs = request.timeout_secs,
                                "retry"
                            );
                            let _ = send_track(&sender, Track::Search(request), &pending).await;
                        }
                    }
                    inflight.fetch_sub(1, Ordering::SeqCst);
                });
            }
        };
        if pending.load(Ordering::SeqCst) == 0 && inflight.load(Ordering::SeqCst) == 0 {
            drop(sender);
            break;
        }
    }
    Ok(())
}

async fn send_tracks(
    sender: &Sender<Track>,
    tracks: Vec<Track>,
    pending: &Arc<AtomicUsize>,
) -> anyhow::Result<()> {
    for track in tracks {
        send_track(sender, track, pending).await?;
    }
    Ok(())
}

async fn send_track(
    sender: &Sender<Track>,
    track: Track,
    pending: &Arc<AtomicUsize>,
) -> anyhow::Result<()> {
    sender
        .send(track)
        .await
        .context("Sending track to manager")?;
    pending.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

fn build_retry(
    managers: &Managers,
    request: SearchRequest,
    reason: RetryReason,
) -> Option<RetryRequest> {
    match reason {
        RetryReason::SearchNoResults => {
            if request.search_attempts >= managers.max_search_retries {
                return None;
            }
            let attempts = request.search_attempts + 1;
            let timeout_secs = capped_backoff(
                managers.search_timeout_secs,
                attempts,
                managers.max_search_timeout_secs,
            );
            let backoff_secs = capped_backoff(
                managers.base_backoff_secs,
                attempts,
                managers.max_backoff_secs,
            );
            let next = SearchRequest {
                item: request.item,
                search_attempts: attempts,
                judge_attempts: request.judge_attempts,
                download_attempts: request.download_attempts,
                timeout_secs,
            };
            Some(RetryRequest {
                target: RetryTarget::Search(next),
                reason: RetryReason::SearchNoResults,
                backoff_secs,
            })
        }
        RetryReason::JudgeNoMatch => {
            if request.judge_attempts >= managers.max_judge_retries {
                return None;
            }
            let attempts = request.judge_attempts + 1;
            let backoff_secs = capped_backoff(
                managers.base_backoff_secs,
                attempts,
                managers.max_backoff_secs,
            );
            let next = SearchRequest {
                item: request.item,
                search_attempts: request.search_attempts,
                judge_attempts: attempts,
                download_attempts: request.download_attempts,
                timeout_secs: managers.search_timeout_secs,
            };
            Some(RetryRequest {
                target: RetryTarget::Search(next),
                reason: RetryReason::JudgeNoMatch,
                backoff_secs,
            })
        }
        RetryReason::DownloadFailed(failure) => {
            if request.download_attempts >= managers.max_download_retries {
                return None;
            }
            let attempts = request.download_attempts + 1;
            let backoff_secs = capped_backoff(
                managers.base_backoff_secs,
                attempts,
                managers.max_backoff_secs,
            );
            let next = SearchRequest {
                item: request.item,
                search_attempts: request.search_attempts,
                judge_attempts: request.judge_attempts,
                download_attempts: attempts,
                timeout_secs: managers.search_timeout_secs,
            };
            Some(RetryRequest {
                target: RetryTarget::Search(next),
                reason: RetryReason::DownloadFailed(failure),
                backoff_secs,
            })
        }
    }
}

fn capped_backoff(base: u64, attempt: u8, max: u64) -> u64 {
    let factor = 2_u64.saturating_pow(attempt as u32);
    let value = base.saturating_mul(factor);
    value.min(max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use soulseek_rs::ClientSettings;

    fn fake_client() -> Arc<soulseek_rs::Client> {
        let settings = ClientSettings {
            username: "user".to_string(),
            password: "pass".to_string(),
            ..Default::default()
        };
        Arc::new(soulseek_rs::Client::with_settings(settings))
    }

    fn base_managers() -> Managers {
        let client = fake_client();
        Managers {
            download_manager: DownloadManager::new_context(client.clone(), PathBuf::from("/tmp")),
            search_manager: SearchManager::new_context(client),
            query_manager: QueryManager::new_context(""),
            judge_manager: JudgeManager::new_context(Arc::new(
                crate::internals::judge::judge_manager::Levenshtein::new(0.5),
            )),
            search_timeout_secs: 10,
            max_search_timeout_secs: 120,
            max_search_retries: 2,
            max_judge_retries: 2,
            max_download_retries: 2,
            base_backoff_secs: 5,
            max_backoff_secs: 60,
            max_concurrent_searches: 1,
            max_concurrent_judges: 1,
            max_concurrent_downloads: 1,
        }
    }

    fn search_request() -> SearchRequest {
        SearchRequest {
            item: SearchItem {
                id: 1,
                track: "Track".to_string(),
                album: "Album".to_string(),
                artist: "Artist".to_string(),
            },
            search_attempts: 0,
            judge_attempts: 0,
            download_attempts: 0,
            timeout_secs: 10,
        }
    }

    #[test]
    fn capped_backoff_limits_and_scales() {
        assert_eq!(capped_backoff(5, 0, 60), 5);
        assert_eq!(capped_backoff(5, 1, 60), 10);
        assert_eq!(capped_backoff(5, 2, 60), 20);
        assert_eq!(capped_backoff(40, 2, 60), 60);
    }

    #[test]
    fn retry_search_no_results_increases_timeout() {
        let managers = base_managers();
        let request = search_request();
        let retry = build_retry(&managers, request, RetryReason::SearchNoResults).expect("retry");
        match retry.target {
            RetryTarget::Search(next) => {
                assert_eq!(next.search_attempts, 1);
                assert_eq!(next.timeout_secs, 20);
            }
        }
    }

    #[test]
    fn retry_judge_no_match_backoff_only() {
        let managers = base_managers();
        let mut request = search_request();
        request.judge_attempts = 1;
        let retry = build_retry(&managers, request, RetryReason::JudgeNoMatch).expect("retry");
        match retry.target {
            RetryTarget::Search(next) => {
                assert_eq!(next.judge_attempts, 2);
                assert_eq!(next.timeout_secs, managers.search_timeout_secs);
            }
        }
    }

    #[test]
    fn retry_download_failed_increments_download_attempts() {
        let managers = base_managers();
        let mut request = search_request();
        request.download_attempts = 1;
        let retry = build_retry(
            &managers,
            request,
            RetryReason::DownloadFailed(DownloadFailure::Failed),
        )
        .expect("retry");
        match retry.target {
            RetryTarget::Search(next) => {
                assert_eq!(next.download_attempts, 2);
            }
        }
    }

    #[test]
    fn retry_respects_limits() {
        let managers = base_managers();
        let mut request = search_request();
        request.search_attempts = managers.max_search_retries;
        assert!(build_retry(&managers, request, RetryReason::SearchNoResults).is_none());
    }
}
