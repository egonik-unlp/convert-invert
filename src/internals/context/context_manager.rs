use std::{path::PathBuf, sync::Arc};

use soulseek_rs::Client;
use tokio::sync::{
    RwLock,
    mpsc::{Receiver, Sender},
};

use anyhow::Context;

use crate::internals::{
    download::download_manager::DownloadManager,
    judge::judge_manager::{JudgeManager, Levenshtein},
    query::query_manager::QueryManager,
    search::search_manager::{
        DownloadableFile, JudgeSubmission, SearchItem, SearchManager, Status,
    },
    utils::config::config_manager::Config,
};

#[allow(dead_code)]
#[derive(Debug)]
pub struct ContextManager {
    status: Receiver<Status>,
}

#[derive(Debug)]
pub struct DownloadedFile {
    pub filename: String,
}

#[allow(dead_code)]
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

impl Managers {
    pub fn new(score: f32, path: PathBuf, config: Config) -> Self {
        let mut client = Client::new(config.user_name, config.user_password);
        client.connect();
        let client = Arc::new(client);
        let download_manager = DownloadManager::new(client.clone(), path);
        let search_manager = SearchManager::new(client.clone());
        let lev_judge = Levenshtein::new(score);
        let judge_manager = JudgeManager::new(Box::new(lev_judge));
        let query_manager = QueryManager::new("ff");
        Managers {
            client,
            download_manager,
            search_manager,
            judge_manager,
            query_manager,
        }
    }
    pub async fn run_cycle(
        self,
        sender: Sender<Track>,
        mut receiver: Receiver<Track>,
    ) -> anyhow::Result<()> {
        let Managers {
            client,
            download_manager,
            search_manager,
            query_manager,
            judge_manager,
        } = self;
        client.login().context("Could not connect")?;
        let storage = Vec::new();
        let state = Arc::new(RwLock::new(storage));
        let tracks = query_manager.run().await.context("Query")?;
        for track in tracks {
            send(track, &sender).await?;
        }
        let mut successful_downloads = vec![];
        while let Some(track) = receiver.recv().await {
            match track {
                Track::Query(search_item) => {
                    let tracks = search_manager
                        .run(search_item)
                        .await
                        .context("returning track")?;
                    for track in tracks {
                        send(track, &sender).await?;
                    }
                }
                Track::Result(judge_submission) => {
                    let track = judge_manager
                        .run(judge_submission)
                        .await
                        .context("Returning judge_submission")?;
                    if let Some(track) = track {
                        send(track, &sender).await.context("returning to main")?
                    }
                }
                Track::Downloadable(judge_submission) => {
                    if state.read().await.contains(&judge_submission) {
                        let track = download_manager
                            .run(judge_submission.clone())
                            .await
                            .context("Downloading")?;
                        if let Some(track_down) = track {
                            send(track_down, &sender)
                                .await
                                .context("returning to main from down")?;
                        }
                    }
                    let mut write = state.write().await;
                    write.push(judge_submission);
                }
                Track::File(downloaded_file) => {
                    tracing::info!(?downloaded_file, "Downloaded file");
                    successful_downloads.push(downloaded_file);
                }
                Track::Retry(retry_request) => {
                    tracing::info!(?retry_request, "Retry requestedfile")
                }
            };
        }
        tracing::info!(?successful_downloads, "Finished all");
        Ok(())
    }
}
