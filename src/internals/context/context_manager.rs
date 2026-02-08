use crate::internals::database::{manager::DatabaseManager, schema};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use soulseek_rs::{Client, ClientSettings};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    fs::OpenOptions,
    io::AsyncWriteExt,
    sync::{
        RwLock, Semaphore,
        mpsc::{Receiver, Sender},
    },
    task::JoinHandle,
};

use anyhow::Context;

use crate::internals::{
    download::download_manager::DownloadManager,
    judge::{judge_manager::JudgeManager, judges::levenshtein::Levenshtein},
    query::query_manager::QueryManager,
    search::search_manager::{DownloadableFile, JudgeSubmission, SearchItem, SearchManager},
    utils::config::config_manager::Config,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct DownloadedFile {
    pub filename: String,
}

#[derive(Debug)]
pub struct RetryRequest {
    pub request: JudgeSubmission,
    pub retry_attempts: u8,
    pub failed_download_result: DownloadableFile,
}

#[derive(Debug)]
pub enum Track {
    Query(SearchItem),
    Result(JudgeSubmission),
    Downloadable(JudgeSubmission),
    File(DownloadedFile),
    Retry(RetryRequest),
    Reject(RejectedTrack),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RejectedTrack {
    track: JudgeSubmission,
    reason: RejectReason,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RejectReason {
    AlreadyDownloaded,
    LowScore(f32),
    NotMusic(String),
    AbandonedAttemptingSearch,
}

impl RejectedTrack {
    pub fn new(track: JudgeSubmission, reason: RejectReason) -> Self {
        Self { track, reason }
    }

    pub fn parts(&self) -> (&JudgeSubmission, &RejectReason) {
        (&self.track, &self.reason)
    }
}

pub trait Manager {
    fn run(self) -> anyhow::Result<()>;
}
pub async fn send(message: Track, chan: &Sender<Track>) -> anyhow::Result<()> {
    chan.send(message).await.context("Send to channel")?;
    Ok(())
}

pub struct Managers {
    pub client: Arc<Client>,
    pub download_manager: DownloadManager,
    pub search_manager: SearchManager,
    pub query_manager: QueryManager,
    pub judge_manager: JudgeManager,
}

#[derive(Debug)]
pub struct RunTools {
    pub download_semaphore: Semaphore,
    pub search_semaphore: Semaphore,
    pub successful_downloads: Vec<Track>,
    pub rejected_tracks: Vec<Track>,
    pub handles: Vec<JoinHandle<anyhow::Result<()>>>,
}

impl RunTools {
    pub fn new(search_limit: usize, download_limit: usize) -> Self {
        let search_semaphore = Semaphore::new(search_limit);
        let download_semaphore = Semaphore::new(download_limit);
        let successful_downloads = vec![];
        let rejected_tracks = vec![];
        let handles = vec![];
        Self {
            search_semaphore,
            download_semaphore,
            successful_downloads,
            rejected_tracks,
            handles,
        }
    }
}

impl Managers {
    pub fn new(score: Option<f32>, path: PathBuf, config: Config) -> Self {
        let client_settings = ClientSettings {
            username: config.user_name,
            password: config.user_password,
            listen_port: config.listen_port,
            ..Default::default()
        };
        let mut client = Client::with_settings(client_settings);
        client.connect();
        let client = Arc::new(client);
        let download_manager = DownloadManager::new(client.clone(), path);
        let search_manager = SearchManager::new(client.clone());
        let lev_judge = Levenshtein::new(score.unwrap_or(0.75));
        let judge_manager = JudgeManager::new(Box::new(lev_judge));
        let query_manager = QueryManager::new(
            "1B3Q6EB9Pjb57jKywHJPfq?si=2f36139519544813",
            config.client_id,
            config.client_secret,
        );
        Managers {
            client,
            download_manager,
            search_manager,
            judge_manager,
            query_manager,
        }
    }
    pub async fn get_playlist(&self) -> Vec<Track> {
        self.query_manager.clone().fetch_playlist().await.unwrap()
    }
    pub async fn inject_tracks(
        track_chunk: impl IntoIterator<Item = Track>,
        sender: Sender<Track>,
    ) -> anyhow::Result<Sender<Track>> {
        for track in track_chunk {
            send(track, &sender).await.unwrap();
        }
        Ok(sender)
    }
    pub async fn run_cycle(
        self,
        sender: Sender<Track>,
        mut receiver: Receiver<Track>,
        connection: &mut PgConnection,
    ) -> anyhow::Result<()> {
        let managers = Arc::new(self);
        let mut database_manager = DatabaseManager::new(connection);

        managers.client.login().context("Could not connect")?;
        let sender = Arc::new(sender);
        let storage = Vec::new();
        let state = Arc::new(RwLock::new(storage));
        let tracks = managers.query_manager.run().await.context("Query")?;
        for track in tracks {
            send(track, &sender).await?;
        }

        let search_semaphore = Arc::new(Semaphore::new(3));
        let download_semaphore = Arc::new(Semaphore::new(5));
        let mut successful_downloads = vec![];
        let mut rejected_tracks = vec![];
        let mut handles = vec![];
        while let Some(track) = receiver.recv().await {
            database_manager
                .load_item_to_database(&track)
                .context("Load into database")?;
            match track {
                Track::Query(search_item) => {
                    let managers = Arc::clone(&managers);
                    let sender = Arc::clone(&sender);
                    let semaphore = search_semaphore.clone();
                    tracing::info!(?search_item, "Enter search_item");
                    let handle: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
                        managers
                            .search_manager
                            .run(search_item, 0, semaphore, sender)
                            .await
                            .context("returning track")?
                            .await
                            .context("inner")?
                            .context("one more")?;
                        Ok(())
                    });
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    handles.push(handle);
                }
                Track::Result(judge_submission) => {
                    let managers = Arc::clone(&managers);
                    let sender = Arc::clone(&sender);
                    let handle: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
                        tracing::info!(?judge_submission, "Enter result");
                        managers
                            .judge_manager
                            .run(judge_submission, sender)
                            .await
                            .context("Returning judge_submission")?;
                        Ok(())
                    });
                    handle.await.context("handle-revisar")?.context("inner")?;
                }
                Track::Downloadable(judge_submission) => {
                    let semaphore = download_semaphore.clone();
                    let managers = Arc::clone(&managers);
                    let sender = Arc::clone(&sender);
                    tracing::info!(?judge_submission, "Enter downloadable");
                    let judge_sub = judge_submission.clone();
                    if !state.read().await.contains(&judge_submission.track) {
                        let handle: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
                            managers
                                .download_manager
                                .run(judge_sub, semaphore, sender)
                                .await
                                .context("Downloading")?;
                            Ok(())
                        });
                        handles.push(handle);
                    } else {
                        let reject = RejectedTrack::new(
                            judge_submission.clone(),
                            RejectReason::AlreadyDownloaded,
                        );
                        send(Track::Reject(reject), &sender)
                            .await
                            .context("sending rejected_tracks")?;
                    }
                    let mut write = state.write().await;
                    write.push(judge_submission.track);
                }
                Track::File(downloaded_file) => {
                    tracing::info!(?downloaded_file, "Downloaded file");
                    successful_downloads.push(downloaded_file);
                    dump_a_vec(&successful_downloads, "successes.json".into())
                        .await
                        .context("dumping succ")?;
                }
                Track::Retry(mut retry_request) => {
                    if retry_request.retry_attempts >= 3 {
                        let reject = RejectedTrack::new(
                            retry_request.request,
                            RejectReason::AbandonedAttemptingSearch,
                        );
                        send(Track::Reject(reject), &sender)
                            .await
                            .context("rejecting")?;
                        continue;
                    }
                    retry_request.retry_attempts += 1;
                    let managers = Arc::clone(&managers);
                    let semaphore = search_semaphore.clone();
                    let sender = Arc::clone(&sender);
                    tracing::info!(?retry_request.request, "Retry zone");
                    let search_item = retry_request.request.clone();
                    let handle: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
                        managers
                            .search_manager
                            .run(search_item.track, 5, semaphore, sender)
                            .await
                            .context("returning track")?;
                        Ok(())
                    });
                    handles.push(handle);
                    tracing::info!(?retry_request, "Retry requestedfile")
                }
                Track::Reject(rejected_track) => {
                    rejected_tracks.push(rejected_track);
                    dump_a_vec(&rejected_tracks, "rejects_dump.json".into())
                        .await
                        .context("dumping")?
                }
            };
        }
        tracing::info!(?successful_downloads, "Finished all");
        for handle in handles {
            handle.await.context("Threads joining")?.context("inner")?;
        }
        Ok(())
    }
}

async fn dump_a_vec<T>(data: &T, filename: String) -> anyhow::Result<()>
where
    T: Serialize,
{
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(filename)
        .await
        .context("Creating rejects file")?;
    let string = serde_json::to_string(data).context("Deserializing rejected track")?;
    file.write_all(string.as_bytes())
        .await
        .context("writing rejects")?;
    Ok(())
}
