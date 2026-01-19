use std::sync::Arc;

use crate::internals::context::context_manager::{DownloadRequest, JudgeResults, SearchResults};
use crate::internals::search::search_manager::JudgeSubmission;
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tracing::instrument;

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
    pub method: Arc<dyn Judge>,
}
impl JudgeManager {
    pub fn new(
        judge_queue: Receiver<JudgeSubmission>,
        download_queue: Sender<JudgeSubmission>,
        method: Arc<dyn Judge>,
    ) -> JudgeManager {
        JudgeManager {
            judge_queue,
            download_queue,
            method,
        }
    }
    pub fn new_context(method: Arc<dyn Judge>) -> JudgeManager {
        let (_judge_tx, judge_queue) = tokio::sync::mpsc::channel(1);
        let (download_queue, _download_rx) = tokio::sync::mpsc::channel(1);
        JudgeManager {
            judge_queue,
            download_queue,
            method,
        }
    }
    pub fn context_clone(&self) -> JudgeManager {
        JudgeManager::new_context(Arc::clone(&self.method))
    }
    #[instrument(name = "JudgeManager::run", skip(self))]
    pub async fn run_channel(self) -> anyhow::Result<()> {
        let mut threads = vec![];
        let JudgeManager {
            mut judge_queue,
            download_queue,
            method,
        } = self;
        while let Some(msg) = judge_queue.recv().await {
            let _method = Arc::clone(&method);
            let download_queue = download_queue.clone();
            let judge_thread: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
                tracing::info!("received in judge manager = {:?}", msg);
                let response = _method
                    .judge(msg.clone())
                    .await
                    .context("awaiting judge response")?;
                if response {
                    download_queue
                        .clone()
                        .send(msg.clone())
                        .await
                        .context("Sending passed file")?;
                    tracing::debug!("Sent to download_queue: {:?}", msg);
                }
                Ok(())
            });
            threads.push(judge_thread);
        }
        // .instrument(span),
        for thread in threads {
            thread
                .await
                .context("joining judgre thread")?
                .context("inner judge")?;
        }
        tracing::debug!("Judge Thread shutting down");
        Ok(())
    }
    pub async fn run(&self, results: SearchResults) -> Result<JudgeResults> {
        let total = results.submissions.len();
        let mut accepted = Vec::new();
        for submission in results.submissions.into_iter() {
            let response = self
                .method
                .judge(submission.clone())
                .await
                .context("awaiting judge response")?;
            if response {
                accepted.push(DownloadRequest {
                    request: results.request.clone(),
                    submission,
                });
            }
        }
        Ok(JudgeResults {
            total,
            request: results.request,
            accepted,
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internals::{
        context::context_manager::{SearchRequest, SearchResults},
        search::search_manager::{DownloadableFile, JudgeSubmission, SearchItem},
    };

    #[derive(Clone)]
    struct TestJudge;

    #[async_trait]
    impl Judge for TestJudge {
        async fn judge(&self, submission: JudgeSubmission) -> anyhow::Result<bool> {
            Ok(submission.query.filename.contains("ok"))
        }

        async fn judge_score(&self, _submission: JudgeSubmission) -> anyhow::Result<f32> {
            Ok(1.0)
        }
    }

    fn search_results() -> SearchResults {
        let request = SearchRequest {
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
        };
        let submissions = vec![
            JudgeSubmission {
                track: request.item.clone(),
                query: DownloadableFile {
                    filename: "not_ok.mp3".to_string(),
                    username: "user".to_string(),
                    size: 1,
                },
            },
            JudgeSubmission {
                track: request.item.clone(),
                query: DownloadableFile {
                    filename: "ok.mp3".to_string(),
                    username: "user".to_string(),
                    size: 1,
                },
            },
        ];
        SearchResults { request, submissions }
    }

    #[tokio::test]
    async fn judge_accepts_matching_submissions() {
        let manager = JudgeManager::new_context(Arc::new(TestJudge));
        let results = search_results();
        let judged = manager.run(results).await.expect("judge");
        assert_eq!(judged.total, 2);
        assert_eq!(judged.accepted.len(), 1);
        assert_eq!(judged.accepted[0].submission.query.filename, "ok.mp3");
    }
}
