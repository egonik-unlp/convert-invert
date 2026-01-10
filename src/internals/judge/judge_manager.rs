use crate::internals::download::download_manager::{DownloadableFile, DownloadableFiles};
use crate::internals::search::search_manager::OwnSearchResult;
use anyhow::Context;
use async_trait::async_trait;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use soulseek_rs::SearchResult;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tracing::{Instrument, instrument};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ResponseFormat {
    score: Option<f32>,
    query_song: Option<String>,
    filename: Option<String>,
}

#[async_trait]
pub trait Judge: Send + Sync {
    async fn judge(&self, track: SearchResult) -> anyhow::Result<bool>;
    async fn judge_score(&self, track: SearchResult) -> anyhow::Result<f32>;
}

pub struct JudgeManager {
    pub judge_queue: Receiver<SearchResult>,
    pub download_queue: Sender<DownloadableFile>,
    pub method: Box<dyn Judge>,
}
impl JudgeManager {
    pub fn new(
        judge_queue: Receiver<SearchResult>,
        download_queue: Sender<DownloadableFile>,
        method: Box<dyn Judge>,
    ) -> JudgeManager {
        JudgeManager {
            judge_queue,
            download_queue,
            method,
        }
    }
    #[instrument(name = "JudgeManager::run", skip(self))]
    pub fn run(mut self) -> anyhow::Result<()> {
        let judge_thread = tokio::spawn(async move {
            loop {
                println!("Awaiting result from search...");
                match self.judge_queue.recv().await {
                    Some(msg) => {
                        let response = self.method.judge(msg.clone()).await.unwrap();
                        if response {
                            let files: DownloadableFiles = msg.clone().into();
                            for file in files.0 {
                                self.download_queue.send(file).await.unwrap();
                            }
                        }
                    }
                    None => {
                        println!("Channel closed. Judge thread exiting.");
                        break;
                    }
                }
            }
        });
        Ok(())
    }

    #[instrument(name = "JudgeManager::run2", skip(self))]
    pub async fn run2(mut self) -> anyhow::Result<()> {
        tracing::info!("en fn out j t");
        let span = tracing::info_span!("judge thread");
        let judge_thread: JoinHandle<anyhow::Result<()>> = tokio::spawn(
            async move {
                while let Some(msg) = self.judge_queue.recv().await {
                    tracing::info!("received in judge manager = {:?}", msg);
                    let response = self
                        .method
                        .judge(msg.clone())
                        .await
                        .context("awaiting judge response")?;
                    if response {
                        let files: DownloadableFiles = msg.clone().into();
                        for file in files.0 {
                            self.download_queue
                                .send(file)
                                .await
                                .context("Sending passed file")?;
                        }
                    }
                }
                Ok(())
            }
            .instrument(span),
        );
        judge_thread
            .await
            .context("joining judgre thread")?
            .context("inner judge")?;
        Ok(())
    }
}

pub struct LocalLLM {
    pub address: String,
    pub port: i32,
    pub score_cutoff: f32,
}
impl LocalLLM {
    pub fn new(address: String, port: i32, score_cutoff: f32) -> Self {
        LocalLLM {
            address,
            port,
            score_cutoff,
        }
    }
    #[instrument(name = "LocalLLM::get_score", skip(self), fields(?track))]
    async fn get_score(&self, track: SearchResult) -> anyhow::Result<ResponseFormat> {
        let own_track: OwnSearchResult = track.into();
        let url = Url::parse(&format!("{}:{}/score", self.address, self.port)).unwrap();
        let client = reqwest::Client::new();
        let str_val = serde_json::to_string(&own_track).context("Parsing own track")?;
        let res = client
            .post(url)
            .header("Content-Type", "application/json")
            .body(str_val)
            .send()
            .await
            .context("Response from llm server")?;
        let text = res.text().await.context("Error reading response")?;
        let response: ResponseFormat = serde_json::from_str(&text).context("parsing response")?;
        Ok(response)
    }
}

#[async_trait]
impl Judge for LocalLLM {
    #[instrument(name = "LocalLLM::judge", skip(self), fields(?track))]
    async fn judge(&self, track: SearchResult) -> anyhow::Result<bool> {
        let response = self.get_score(track).await.context("Getting score")?;
        match response.score {
            x if x.unwrap_or_default() > self.score_cutoff => Ok(true),
            _ => Ok(false),
        }
    }

    #[instrument(name = "LocalLLM::judge", skip(self), fields(?track))]
    async fn judge_score(&self, track: SearchResult) -> anyhow::Result<f32> {
        let response = self.get_score(track).await.context("Getting score")?;
        Ok(response.score.unwrap_or_default())
    }
}

pub struct Levenshtein {
    pub score_cutoff: f32,
}
impl Levenshtein {}

#[async_trait]
impl Judge for Levenshtein {
    async fn judge(&self, track: SearchResult) -> anyhow::Result<bool> {
        todo!()
    }
    async fn judge_score(&self, track: SearchResult) -> anyhow::Result<f32> {
        let own_track: OwnSearchResult = track.into();
        let str_val = serde_json::to_string(&own_track).unwrap();
        todo!()
    }
}
