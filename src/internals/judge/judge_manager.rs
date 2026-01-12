use crate::internals::search::search_manager::JudgeSubmission;
use anyhow::{Context, Ok};
use async_trait::async_trait;
use reqwest::Url;
use serde::{Deserialize, Serialize};
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
    async fn judge(&self, submission: JudgeSubmission) -> anyhow::Result<bool>;
    async fn judge_score(&self, submission: JudgeSubmission) -> anyhow::Result<f32>;
}

pub struct JudgeManager {
    pub judge_queue: Receiver<JudgeSubmission>,
    pub download_queue: Sender<JudgeSubmission>,
    pub method: Box<dyn Judge>,
}
impl JudgeManager {
    pub fn new(
        judge_queue: Receiver<JudgeSubmission>,
        download_queue: Sender<JudgeSubmission>,
        method: Box<dyn Judge>,
    ) -> JudgeManager {
        JudgeManager {
            judge_queue,
            download_queue,
            method,
        }
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
                        self.download_queue
                            .send(msg)
                            .await
                            .context("Sending passed file")?;
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
    #[instrument(name = "LocalLLM::get_score", skip(self, submission))]
    async fn get_score(&self, submission: JudgeSubmission) -> anyhow::Result<ResponseFormat> {
        let url = Url::parse(&format!("{}:{}/score", self.address, self.port)).unwrap();
        let client = reqwest::Client::new();
        let str_val = serde_json::to_string(&submission).context("Parsing own track")?;
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
    #[instrument(name = "LocalLLM::judge", skip(self), fields(username = submission.query.username , query_song = submission.track.track, file_q = submission.query.filename))]
    async fn judge(&self, submission: JudgeSubmission) -> anyhow::Result<bool> {
        let response = self.get_score(submission).await.context("Getting score")?;
        tracing::info!("{:?}", response);
        match response.score {
            x if x.unwrap_or_default() > self.score_cutoff => Ok(true),
            _ => Ok(false),
        }
    }

    #[instrument(name = "LocalLLM::judge", skip(self), fields(username = submission.query.username , query_song = submission.track.track, file_q = submission.query.filename))]
    async fn judge_score(&self, submission: JudgeSubmission) -> anyhow::Result<f32> {
        let response = self.get_score(submission).await.context("Getting score")?;
        Ok(response.score.unwrap_or_default())
    }
}

pub struct Levenshtein {
    pub score_cutoff: f32,
}
impl Levenshtein {
    pub fn new(score_cutoff: f32) -> Self {
        Levenshtein { score_cutoff }
    }
}

#[async_trait]
impl Judge for Levenshtein {
    #[instrument(name = "Levenshtein::judge", skip(self, submission), fields(username = submission.query.username , query_song = submission.track.track, file_q = submission.query.filename))]
    async fn judge(&self, submission: JudgeSubmission) -> anyhow::Result<bool> {
        let distance = str_distance::Levenshtein::default();
        let a = format!("{}", submission.track);
        let b = submission.query.filename;
        let distance_val = str_distance::str_distance_normalized(a, b, distance);
        tracing::info!("score = {}", distance_val);
        let val = distance_val > self.score_cutoff as f64;
        Ok(val)
    }
    #[instrument(name = "Levenshtein::judge_score", skip(self,submission), fields(username = submission.query.username , query_song = submission.track.track, file_q = submission.query.filename))]
    async fn judge_score(&self, submission: JudgeSubmission) -> anyhow::Result<f32> {
        let distance = str_distance::Levenshtein::default();
        let a = format!("{}", submission.track);
        let b = submission.query.filename;
        let distance_val = str_distance::str_distance_normalized(a, b, distance);
        tracing::info!("score = {}", distance_val);
        Ok(distance_val as f32)
    }
}
