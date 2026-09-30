use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

use crate::internals::{
    context::context_manager::{RejectReason, RejectedTrack, Track, send},
    judge::judges::relative_mi::RelativeMi,
    search::search_manager::JudgeSubmission,
};

/// Single source of truth for the judge's accept/reject cutoff. The HTTP API
/// surfaces this value at `/api/config` so the frontend can render the same
/// threshold the backend actually applies — previously the two drifted (0.75
/// in code, 0.85 in the UI).
pub const JUDGE_THRESHOLD: f32 = 0.75;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ResponseFormat {
    pub score: Option<f32>,
    pub query_song: Option<String>,
    pub filename: Option<String>,
}

/// One judge's verdict on one candidate, kept for the judgement log and the UI.
///
/// The pipeline only ever routes on `score`, but a score alone is a poor record
/// of *why* a file was rejected. An LLM judge answers with a label
/// ("wrong_version"), and that label — plus which model said it, how long it
/// took, and what it literally replied — is the part worth persisting and
/// showing a human.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Judgement {
    /// Which judge produced it: `levenshtein`, `relative_mi`, `smol-judge`.
    pub judge: String,
    /// The served model, for judges that have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The verdict in the judge's own vocabulary. `None` for judges that only
    /// produce a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub score: f32,
    /// Whether this verdict on its own clears the manager's threshold. Set by
    /// [`JudgeManager`], which owns the threshold.
    #[serde(default)]
    pub accepted: bool,
    pub latency_ms: i32,
    /// The judge's literal reply, for auditing a surprising verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Set when the judge failed and a fallback score was substituted, so a
    /// degraded run is visible in the log instead of looking like a real
    /// verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The prompt contract the verdict was produced under, so a later prompt
    /// change is legible in the log rather than silently mixed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_version: Option<String>,
}

impl Judgement {
    pub fn new(judge: impl Into<String>, score: f32) -> Self {
        Self {
            judge: judge.into(),
            model: None,
            label: None,
            score,
            accepted: false,
            latency_ms: 0,
            raw: None,
            error: None,
            prompt_version: None,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_prompt_version(mut self, version: impl Into<String>) -> Self {
        self.prompt_version = Some(version.into());
        self
    }

    pub fn with_latency(mut self, latency_ms: i32) -> Self {
        self.latency_ms = latency_ms;
        self
    }
}

#[async_trait]
pub trait Judge: Send + Sync {
    /// Stable identifier recorded with every judgement this judge produces.
    fn name(&self) -> &'static str;
    async fn judge(&self, submission: JudgeSubmission) -> anyhow::Result<bool>;
    async fn judge_score(&self, submission: JudgeSubmission) -> anyhow::Result<f32>;
    async fn judge_block(&self, submissions: Vec<JudgeSubmission>) -> anyhow::Result<Vec<f32>>;

    /// The verdict with everything the judge can say about it.
    ///
    /// Judges that only produce a number get this for free: the default times
    /// [`Judge::judge_score`] and wraps it. Judges with more to report — a
    /// label, a model, a parse failure — override it.
    async fn judge_detailed(&self, submission: JudgeSubmission) -> anyhow::Result<Judgement> {
        let started = Instant::now();
        let score = self.judge_score(submission).await?;
        let latency_ms = started.elapsed().as_millis().min(i32::MAX as u128) as i32;
        Ok(Judgement::new(self.name(), score).with_latency(latency_ms))
    }
}

pub struct JudgeManager {
    pub method: Box<dyn Judge>,
    pub experimental_method: Box<dyn Judge>,
    pub threshold: f32,
}
impl JudgeManager {
    pub fn new(method: Box<dyn Judge>, threshold: f32) -> JudgeManager {
        JudgeManager {
            method,
            experimental_method: Box::new(RelativeMi::new()),
            threshold,
        }
    }
    pub async fn run(
        &self,
        track: JudgeSubmission,
        sender: Arc<Sender<Track>>,
    ) -> anyhow::Result<()> {
        tracing::debug!(?track, "Received judge submission");
        let mut inner_track = track.clone();
        let mut primary = self
            .method
            .judge_detailed(track.clone())
            .await
            .context("awaiting judge response")?;
        let mut experimental = self
            .experimental_method
            .judge_detailed(track.clone())
            .await
            .context("awaiting experimental judge response")?;
        // The judges report a score; deciding what clears the bar is the
        // manager's job, so it stamps `accepted` rather than each judge guessing.
        primary.accepted = primary.score > self.threshold;
        experimental.accepted = experimental.score > self.threshold;

        let response = primary.score;
        inner_track.score = Some(response);
        inner_track.relative_mi_score = Some(experimental.score);
        for judgement in [&primary, &experimental] {
            tracing::info!(
                judge = %judgement.judge,
                model = judgement.model.as_deref().unwrap_or("-"),
                label = judgement.label.as_deref().unwrap_or("-"),
                score = judgement.score,
                accepted = judgement.accepted,
                latency_ms = judgement.latency_ms,
                error = judgement.error.as_deref().unwrap_or(""),
                track_id = %track.track.track_id,
                artist = %track.track.artist,
                title = %track.track.track,
                filename = %track.query.filename,
                "Judgement",
            );
        }
        // Only the deciding judge is persisted as a judgement. The experimental
        // score already has its own column on the submission, and logging it as
        // a second verdict would double every row in the judgements feed and
        // halve every rate computed from it.
        //
        // Carried on the submission so the database manager can persist it on
        // whichever branch this candidate takes next (accepted or rejected).
        inner_track.judgements = vec![primary];

        if response > self.threshold {
            send(Track::Downloadable(inner_track), &sender)
                .await
                .context("sending judgement")?;
        } else {
            let reject = RejectedTrack::new(inner_track, RejectReason::LowScore(response));
            send(Track::Reject(reject), &sender)
                .await
                .context("sending reject")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Judge, JudgeManager};
    use crate::internals::{
        context::context_manager::{RejectReason, Track},
        search::search_manager::{DownloadableFile, JudgeSubmission, SearchItem},
    };
    use async_trait::async_trait;
    use std::sync::Arc;

    struct FixedJudge {
        score: f32,
    }

    #[async_trait]
    impl Judge for FixedJudge {
        fn name(&self) -> &'static str {
            "fixed"
        }

        async fn judge(&self, _submission: JudgeSubmission) -> anyhow::Result<bool> {
            Ok(self.score > 0.0)
        }

        async fn judge_score(&self, _submission: JudgeSubmission) -> anyhow::Result<f32> {
            Ok(self.score)
        }

        async fn judge_block(&self, submissions: Vec<JudgeSubmission>) -> anyhow::Result<Vec<f32>> {
            Ok(vec![self.score; submissions.len()])
        }
    }

    fn submission() -> JudgeSubmission {
        JudgeSubmission {
            track: SearchItem::new(
                "spotify-track-id".to_string(),
                "Track".to_string(),
                "Album".to_string(),
                "Artist".to_string(),
            ),
            query: DownloadableFile {
                filename: "Artist - Track.flac".to_string(),
                username: "user".to_string(),
                size: 1024,
            },
            score: None,
            relative_mi_score: None,
            judgements: Vec::new(),
        }
    }

    #[tokio::test]
    async fn manager_uses_configured_threshold_for_acceptance() {
        let manager = JudgeManager::new(Box::new(FixedJudge { score: 0.8 }), 0.75);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);

        manager
            .run(submission(), Arc::new(sender))
            .await
            .expect("judge run");

        match receiver.recv().await.expect("message") {
            Track::Downloadable(track) => {
                assert_eq!(track.score, Some(0.8));
                assert!(track.relative_mi_score.is_some());
                // Only the deciding judge is logged — one verdict per candidate.
                let judges: Vec<&str> = track
                    .judgements
                    .iter()
                    .map(|judgement| judgement.judge.as_str())
                    .collect();
                assert_eq!(judges, vec!["fixed"]);
                assert!(track.judgements[0].accepted);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn manager_rejects_scores_at_or_below_threshold() {
        let manager = JudgeManager::new(Box::new(FixedJudge { score: 0.75 }), 0.75);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);

        manager
            .run(submission(), Arc::new(sender))
            .await
            .expect("judge run");

        match receiver.recv().await.expect("message") {
            Track::Reject(track) => {
                let (submission, reason) = track.parts();
                assert!(matches!(reason, RejectReason::LowScore(score) if *score == 0.75));
                assert!(submission.relative_mi_score.is_some());
                // A rejected candidate is still judged, and the verdict is kept:
                // "why was this not downloaded" is the question the log answers.
                assert!(!submission.judgements.is_empty());
                assert!(!submission.judgements[0].accepted);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
}
