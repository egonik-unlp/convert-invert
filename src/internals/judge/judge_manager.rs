use std::sync::Arc;

use anyhow::{Context, Ok};
use async_trait::async_trait;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;
use tracing::instrument;

use crate::internals::{
    context::context_manager::{RejectReason, RejectedTrack, Track, send},
    search::search_manager::JudgeSubmission,
};

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
    pub method: Box<dyn Judge>,
}
impl JudgeManager {
    pub fn new(method: Box<dyn Judge>) -> JudgeManager {
        JudgeManager { method }
    }
    pub async fn run(
        &self,
        track: JudgeSubmission,
        sender: Arc<Sender<Track>>,
    ) -> anyhow::Result<()> {
        tracing::info!("received in judge manager = {:?}", track);
        let response = self
            .method
            .judge_score(track.clone())
            .await
            .context("awaiting judge response")?;
        if response > 0.75 {
            send(Track::Downloadable(track), &sender)
                .await
                .context("sending judgement")?;
        } else {
            let reject = RejectedTrack::new(track, RejectReason::LowScore(response));
            send(Track::Reject(reject), &sender)
                .await
                .context("sending reject")?;
        }
        Ok(())
    }
}

#[derive(Clone)]
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

#[derive(Clone)]
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
    #[instrument(name = "Levenshtein::judge", skip(self, submission), fields(id=submission.track.id,username = submission.query.username , query_song = submission.track.track, file_q = submission.query.filename))]
    async fn judge(&self, submission: JudgeSubmission) -> anyhow::Result<bool> {
        let distance = str_distance::Levenshtein::default();
        let a = format!("{}", submission.track);
        let b = submission.query.filename;
        let distance_val = str_distance::str_distance_normalized(a, b, distance);
        tracing::info!("score = {}", distance_val);
        let val = distance_val > self.score_cutoff as f64;
        Ok(val)
    }
    #[instrument(name = "Levenshtein::judge_score", skip(self,submission), fields(id=submission.track.id,username = submission.query.username , query_song = submission.track.track, file_q = submission.query.filename))]
    async fn judge_score(&self, submission: JudgeSubmission) -> anyhow::Result<f32> {
        let distance = str_distance::Levenshtein::default();
        let a = format!("{}", submission.track);
        let b = submission.query.filename;
        let distance_val = str_distance::str_distance_normalized(a, b, distance);
        tracing::info!("score = {}", distance_val);
        Ok(distance_val as f32)
    }
}
