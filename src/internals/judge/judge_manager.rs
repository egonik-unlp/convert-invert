use crate::internals::download::download_manager::{DownloadableFile, DownloadableFiles};
use crate::internals::search::threaded_search::OwnSearchResult;
use async_trait::async_trait;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use soulseek_rs::SearchResult;
use tokio::sync::mpsc::{Receiver, Sender};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ResponseFormat {
    score: Option<f32>,
    query_song: Option<String>,
    filename: Option<String>,
}

#[async_trait]
pub trait Judge: Send + Sync {
    async fn judge(&self, track: SearchResult) -> anyhow::Result<bool>;
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
    pub fn run(mut self) -> anyhow::Result<()> {
        let judge_thread = tokio::spawn(async move {
            println!("Runing judge_thread\n\n\n");
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

    pub async fn run2(mut self) -> anyhow::Result<()> {
        let judge_thread = tokio::spawn(async move {
            println!("Runing judge_thread\n\n\n");
            while let Some(msg) = self.judge_queue.recv().await {
                println!("received in judge manager = {:?}", msg);
                let response = self.method.judge(msg.clone()).await.unwrap();
                if response {
                    let files: DownloadableFiles = msg.clone().into();
                    for file in files.0 {
                        self.download_queue.send(file).await.unwrap();
                    }
                }
            }
        });
        judge_thread.await.unwrap();
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
}

#[async_trait]
impl Judge for LocalLLM {
    async fn judge(&self, track: SearchResult) -> anyhow::Result<bool> {
        let own_track: OwnSearchResult = track.into();
        let url = Url::parse(&format!("{}:{}/score", self.address, self.port)).unwrap();
        let client = reqwest::Client::new();
        let str_val = serde_json::to_string(&own_track).unwrap();
        let res = client
            .post(url)
            .header("Content-Type", "application/json")
            .body(str_val)
            .send()
            .await
            .unwrap();
        let text = res.text().await.unwrap();
        let response: ResponseFormat = serde_json::from_str(&text).unwrap();
        match response.score {
            x if x.unwrap_or_default() > self.score_cutoff => Ok(true),
            _ => Ok(false),
        }
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
}
