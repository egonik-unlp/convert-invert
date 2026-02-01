use async_trait::async_trait;
use tracing::instrument;

use crate::internals::{judge::judge_manager::Judge, search::search_manager::JudgeSubmission};

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
