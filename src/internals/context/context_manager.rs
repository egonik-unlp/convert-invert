use tokio::sync::mpsc::{Receiver, Sender};

use anyhow::Context;
use soulseek_rs::Client;
use tokio::sync::mpsc::{self, Receiver, Sender};

use crate::internals::{
    download::download_manager::DownloadManager,
    judge::judge_manager::JudgeManager,
    query::query_manager::QueryManager,
    search::search_manager::{
        DownloadableFile, JudgeSubmission, SearchItem, SearchManager, Status,
    },
    utils::config,
};

#[allow(dead_code)]
#[derive(Debug)]
pub struct ContextManager {
    status: Receiver<Status>,
}

#[derive(Debug)]
pub struct DownloadedFile;

#[allow(dead_code)]
#[derive(Debug)]
pub struct RetryRequest {
    request: JudgeSubmission,
    retry_attempts: u8,
    failed_download_result: DownloadableFile,
}

#[derive(Debug)]
pub enum Track {
    Query(Vec<SearchItem>),
    Result(JudgeSubmission),
    Downloadable(JudgeSubmission),
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
    pub sender: Sender<Track>,
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
) -> anyhow::Result<()> {
    while let Some(track) = receiver.recv().await {
        match track {
            Track::Query(search_item) => {
                let x = Managers::new(75)
                    .query_manager
                    .run()
                    .await
                    .context("Query")?;
                sender.clone().send(x).await.unwrap();
            }
            Track::Result(judge_submission) => todo!(),
            Track::Downloadable(judge_submission) => todo!(),
            Track::File(downloaded_file) => todo!(),
            Track::Retry(retry_request) => todo!(),
        };
    }
    Ok(())
}
