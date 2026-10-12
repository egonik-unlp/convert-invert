//! The fine-tuned SmolLM judge, served by ollama.
//!
//! `eduardogonik/smol-judge:c200-q8` is a 135M SmolLM2 fine-tuned on ~12,200
//! (Spotify metadata, Soulseek filename) pairs to answer one question: is this
//! file the recording that was asked for? It answers with a *label*, not a
//! similarity number, which is the whole point — the Levenshtein judge cannot
//! tell "Only Human" from "Only Human (Skream Remix)", because as strings they
//! are nearly the same. The label is also what the UI shows a human.
//!
//! The prompt here is a byte-for-byte copy of the one the model was trained
//! with (`src/prompt.ts` in the smol-model-test repo, `PROMPT_VERSION = "p1"`).
//! Serving drift is the failure mode this module has to avoid, so if that
//! prompt ever changes, [`PROMPT_VERSION`] must change with it — every stored
//! judgement records the version it was produced under.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tracing::instrument;

use crate::internals::{
    judge::{
        judge_manager::{Judge, Judgement},
        judges::{
            levenshtein::Levenshtein,
            rules::{self, Tier},
        },
    },
    search::search_manager::JudgeSubmission,
};

/// Name recorded in the `judgements.judge` column for this judge.
pub const JUDGE_NAME: &str = "smol-judge";

/// The prompt contract this judge serves under. Bump when the prompt changes.
pub const PROMPT_VERSION: &str = "p1";

pub const DEFAULT_MODEL: &str = "eduardogonik/smol-judge:c200-q8";
/// From inside the api container the host's ollama is reachable over the docker
/// gateway; `extra_hosts` in compose maps the name.
pub const DEFAULT_URL: &str = "http://host.docker.internal:11434";

/// Deliberately short. The model learned the rules from the training set and
/// does not need them restated; a full rulebook here would be both wasted
/// tokens and a divergence from how it was trained.
const SYSTEM_PROMPT: &str = "Judge whether a music search result is the requested recording.";

/// What the judge does when ollama cannot be reached or answers unusably.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackMode {
    /// Score the candidate with the Levenshtein judge instead. The pipeline
    /// keeps running at the old quality rather than stalling, and the stored
    /// judgement carries the error so the degradation is visible.
    Levenshtein,
    /// Score 0, i.e. reject. Choose this when a wrong download costs more than
    /// a missed one.
    Reject,
}

impl FallbackMode {
    fn from_env_value(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "reject" | "zero" | "none" => Self::Reject,
            _ => Self::Levenshtein,
        }
    }
}

/// The four labels the student was trained on.
///
/// `not_music` and `uncertain` exist in the labelling taxonomy but were never
/// taught to this model, so they are not representable here — the ollama
/// response schema below refuses them too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Label {
    /// The requested recording. "(Original Mix)", "(Album Version)" or no
    /// version tag at all all count as the original.
    Exact,
    /// Right song, wrong recording: an unrequested remix/live/edit, or the
    /// plain original when a specific remix was asked for.
    WrongVersion,
    /// Title matches, performing artist is a different act.
    WrongArtist,
    /// An unrelated track.
    Different,
}

impl Label {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::WrongVersion => "wrong_version",
            Self::WrongArtist => "wrong_artist",
            Self::Different => "different",
        }
    }

    /// How good a download this label implies.
    ///
    /// `wrong_version` sits above zero because a remix of the right song beats
    /// an unrelated file as a last resort, but it is far below the pipeline's
    /// 0.75 accept threshold, so only `exact` is ever downloaded.
    pub fn score(self) -> f32 {
        match self {
            Self::Exact => 1.0,
            Self::WrongVersion => 0.3,
            Self::WrongArtist => 0.05,
            Self::Different => 0.0,
        }
    }
}

/// The user-side prompt, rendered exactly as in training.
pub fn build_user_prompt(artist: &str, track: &str, album: &str, filename: &str) -> String {
    format!(
        "Requested:\n  artist: {artist}\n  track:  {track}\n  album:  {album}\n\nCandidate file:\n  {filename}"
    )
}

fn output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "label": { "type": "string", "enum": ["exact", "wrong_version", "wrong_artist", "different"] },
            "score": { "type": "number", "minimum": 0, "maximum": 1 },
        },
        "required": ["label", "score"],
    })
}

#[derive(Deserialize)]
struct GenerateResponse {
    #[serde(default)]
    response: String,
}

#[derive(Deserialize)]
struct ModelVerdict {
    label: Label,
    #[serde(default)]
    score: Option<f32>,
}

/// Parses the model reply. Ollama's structured-output mode makes this reliable,
/// but a model can still emit a code fence or trailing prose, so the JSON object
/// is located rather than assumed to be the whole string.
fn parse_verdict(raw: &str) -> Option<ModelVerdict> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<ModelVerdict>(&raw[start..=end]).ok()
}

#[derive(Clone)]
pub struct SmolJudge {
    http: reqwest::Client,
    url: String,
    model: String,
    keep_alive: String,
    /// Ollama serves one request at a time per model by default; queueing more
    /// than a few in parallel just adds latency, and a 200-candidate track can
    /// otherwise open 200 sockets at once. Per judge instance, so the real
    /// ceiling is this times the worker count.
    permits: Arc<Semaphore>,
    fallback_mode: FallbackMode,
    fallback: Levenshtein,
    /// Run the deterministic rule engine before asking the model. On by
    /// default; set `SMOL_JUDGE_RULES=0` to measure the model on its own.
    rules_enabled: bool,
    /// CPU threads Ollama may use per request (`SMOL_JUDGE_NUM_THREAD`, default 2; 0 leaves it
    /// to Ollama). llama.cpp degrades badly when its threads wait on busy cores: on a loaded
    /// 6-core host the default thread count took ~1.5 s/token versus ~8 ms/token with 2.
    num_thread: usize,
}

impl SmolJudge {
    pub fn new(url: String, model: String, score_cutoff: f32) -> anyhow::Result<Self> {
        Self::with_options(
            url,
            model,
            score_cutoff,
            env_usize("SMOL_JUDGE_CONCURRENCY", 2).max(1),
            Duration::from_secs(env_u64("SMOL_JUDGE_TIMEOUT_SECS", 30).max(1)),
            std::env::var("SMOL_JUDGE_KEEP_ALIVE").unwrap_or_else(|_| "30m".to_string()),
            std::env::var("SMOL_JUDGE_FALLBACK")
                .map(|value| FallbackMode::from_env_value(&value))
                .unwrap_or(FallbackMode::Levenshtein),
        )
    }

    pub fn with_options(
        url: String,
        model: String,
        score_cutoff: f32,
        concurrency: usize,
        timeout: Duration,
        keep_alive: String,
        fallback_mode: FallbackMode,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("building HTTP client for the smol judge")?;
        Ok(Self {
            http,
            url: url.trim_end_matches('/').to_string(),
            model,
            keep_alive,
            permits: Arc::new(Semaphore::new(concurrency)),
            fallback_mode,
            fallback: Levenshtein::new(score_cutoff),
            rules_enabled: std::env::var("SMOL_JUDGE_RULES")
                .map(|v| {
                    !matches!(
                        v.trim().to_ascii_lowercase().as_str(),
                        "0" | "false" | "off"
                    )
                })
                .unwrap_or(true),
            num_thread: env_usize("SMOL_JUDGE_NUM_THREAD", 2),
        })
    }

    /// Run without the rule engine.
    ///
    /// Used by tests that need to exercise the model path itself, and as an
    /// escape hatch for measuring the model unaided. `SMOL_JUDGE_RULES=0` does
    /// the same from the environment.
    pub fn with_rules(mut self, enabled: bool) -> Self {
        self.rules_enabled = enabled;
        self
    }

    pub fn default_url() -> String {
        DEFAULT_URL.to_string()
    }

    pub fn default_model() -> String {
        DEFAULT_MODEL.to_string()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// One round trip to ollama. Returns the label and the model's own score,
    /// which is kept only for the log — the score the pipeline routes on is
    /// derived from the label, so a garbled number cannot flip a decision.
    async fn ask(&self, submission: &JudgeSubmission) -> anyhow::Result<(Label, String)> {
        let _permit = self
            .permits
            .acquire()
            .await
            .context("acquiring smol judge permit")?;
        let mut body = serde_json::json!({
            "model": self.model,
            "system": SYSTEM_PROMPT,
            "prompt": build_user_prompt(
                &submission.track.artist,
                &submission.track.track,
                &submission.track.album,
                &submission.query.filename,
            ),
            "stream": false,
            "think": false,
            "keep_alive": self.keep_alive,
            // Greedy and seeded: the same candidate must get the same verdict on
            // a re-run, otherwise the stored judgement log is not reproducible.
            "options": { "temperature": 0, "top_p": 1, "seed": 42, "num_predict": 32 },
            "format": output_schema(),
        });
        if self.num_thread > 0 {
            body["options"]["num_thread"] = serde_json::json!(self.num_thread);
        }

        let response = self
            .http
            .post(format!("{}/api/generate", self.url))
            .header("content-type", "application/json")
            .body(serde_json::to_string(&body).context("serializing the generate request")?)
            .send()
            .await
            .context("ollama generate request")?;
        let status = response.status();
        let text = response.text().await.context("reading ollama response")?;
        if !status.is_success() {
            anyhow::bail!(
                "ollama {status}: {}",
                text.chars().take(200).collect::<String>()
            );
        }
        let parsed: GenerateResponse =
            serde_json::from_str(&text).context("parsing ollama envelope")?;
        let raw = parsed.response.trim().to_string();
        let verdict =
            parse_verdict(&raw).with_context(|| format!("unparseable judge reply: {raw:?}"))?;
        let raw_log = match verdict.score {
            Some(score) => format!(
                "{{\"label\":\"{}\",\"score\":{score}}}",
                verdict.label.as_str()
            ),
            None => raw,
        };
        Ok((verdict.label, raw_log))
    }

    /// The score used when the model is unreachable, per [`FallbackMode`].
    async fn fallback_score(&self, submission: JudgeSubmission) -> f32 {
        match self.fallback_mode {
            FallbackMode::Reject => 0.0,
            FallbackMode::Levenshtein => self
                .fallback
                .judge_score(submission)
                .await
                .unwrap_or_default(),
        }
    }
}

#[async_trait]
impl Judge for SmolJudge {
    fn name(&self) -> &'static str {
        JUDGE_NAME
    }

    #[instrument(
        name = "SmolJudge::judge_detailed",
        skip(self, submission),
        fields(
            id = submission.track.track_id,
            username = submission.query.username,
            query_song = submission.track.track,
            file_q = submission.query.filename,
        ),
    )]
    async fn judge_detailed(&self, submission: JudgeSubmission) -> anyhow::Result<Judgement> {
        let started = Instant::now();

        // The rule engine first. It settles ~83% of real candidates with no
        // inference, and on the held-out set the pair beat the model alone
        // (96.0% vs 93.6% accuracy). Only `Gold` short-circuits: `Weak` tiers
        // exist because an audit showed them unreliable, so those still go to
        // the model.
        if self.rules_enabled {
            let verdict = rules::classify(
                &submission.track.artist,
                &submission.track.track,
                &submission.track.album,
                &submission.query.filename,
            );
            if verdict.tier == Tier::Gold {
                if let Some(label) = verdict.label {
                    let latency_ms = started.elapsed().as_millis().min(i32::MAX as u128) as i32;
                    tracing::debug!(
                        label = label.as_str(),
                        rule = verdict.rule,
                        "Smol judge settled by rule, no inference",
                    );
                    let mut judgement = Judgement::new(JUDGE_NAME, label.score())
                        .with_model(format!("{}+rules", self.model))
                        .with_prompt_version(PROMPT_VERSION)
                        .with_latency(latency_ms);
                    judgement.label = Some(label.as_str().to_string());
                    judgement.raw = Some(format!(
                        "{{\"label\":\"{}\",\"rule\":\"{}\"}}",
                        label.as_str(),
                        verdict.rule
                    ));
                    return Ok(judgement);
                }
            }
        }

        let outcome = self.ask(&submission).await;
        let latency_ms = started.elapsed().as_millis().min(i32::MAX as u128) as i32;

        let mut judgement = Judgement::new(JUDGE_NAME, 0.0)
            .with_model(self.model.clone())
            .with_prompt_version(PROMPT_VERSION)
            .with_latency(latency_ms);

        match outcome {
            Ok((label, raw)) => {
                tracing::debug!(label = label.as_str(), latency_ms, "Smol judge verdict");
                judgement.score = label.score();
                judgement.label = Some(label.as_str().to_string());
                judgement.raw = Some(raw);
            }
            Err(error) => {
                // A model outage must not take the pipeline down with it, so the
                // error is recorded on the judgement and the configured fallback
                // supplies the score.
                let detail = format!("{error:#}");
                tracing::warn!(
                    error = %detail,
                    fallback = ?self.fallback_mode,
                    "Smol judge unavailable; falling back",
                );
                judgement.score = self.fallback_score(submission).await;
                judgement.error = Some(detail);
            }
        }
        Ok(judgement)
    }

    #[instrument(name = "SmolJudge::judge", skip(self, submission))]
    async fn judge(&self, submission: JudgeSubmission) -> anyhow::Result<bool> {
        Ok(matches!(self.ask(&submission).await?.0, Label::Exact))
    }

    #[instrument(name = "SmolJudge::judge_score", skip(self, submission))]
    async fn judge_score(&self, submission: JudgeSubmission) -> anyhow::Result<f32> {
        Ok(self.judge_detailed(submission).await?.score)
    }

    #[instrument(name = "SmolJudge::judge_block", skip(self, submissions))]
    async fn judge_block(&self, submissions: Vec<JudgeSubmission>) -> anyhow::Result<Vec<f32>> {
        let mut scores = Vec::with_capacity(submissions.len());
        for submission in submissions {
            scores.push(self.judge_score(submission).await?);
        }
        Ok(scores)
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::{Label, build_user_prompt, parse_verdict};

    #[test]
    fn prompt_matches_the_training_rendering() {
        let prompt = build_user_prompt("KH", "Only Human", "Only Human", "01 - Only Human.flac");

        assert_eq!(
            prompt,
            "Requested:\n  artist: KH\n  track:  Only Human\n  album:  Only Human\n\nCandidate file:\n  01 - Only Human.flac",
        );
    }

    #[test]
    fn parses_a_plain_structured_reply() {
        let verdict = parse_verdict(r#"{"label":"exact","score":1}"#).expect("verdict");

        assert_eq!(verdict.label, Label::Exact);
        assert_eq!(verdict.score, Some(1.0));
    }

    #[test]
    fn parses_a_reply_wrapped_in_a_code_fence() {
        let verdict = parse_verdict("```json\n{\"label\":\"wrong_version\",\"score\":0.3}\n```")
            .expect("verdict");

        assert_eq!(verdict.label, Label::WrongVersion);
    }

    #[test]
    fn rejects_a_label_outside_the_trained_taxonomy() {
        assert!(parse_verdict(r#"{"label":"not_music","score":0}"#).is_none());
        assert!(parse_verdict("no json here").is_none());
    }

    /// The fallback is the difference between "ollama restarted" and "the run
    /// died", so it is worth a test that does not need ollama: port 1 on
    /// loopback refuses immediately.
    #[tokio::test]
    async fn an_unreachable_model_falls_back_instead_of_failing_the_pipeline() {
        use super::{FallbackMode, SmolJudge};
        use crate::internals::judge::judge_manager::Judge;
        use crate::internals::search::search_manager::{
            DownloadableFile, JudgeSubmission, SearchItem,
        };
        use std::time::Duration;

        let submission = || {
            JudgeSubmission::new(
                SearchItem::new(
                    "t".to_string(),
                    "Only Human".to_string(),
                    "Only Human".to_string(),
                    "KH".to_string(),
                ),
                DownloadableFile {
                    filename: "KH - Only Human.flac".to_string(),
                    username: "peer".to_string(),
                    size: 1024,
                },
            )
        };
        let build = |mode| {
            SmolJudge::with_options(
                "http://127.0.0.1:1".to_string(),
                "smol-judge:c200-q8".to_string(),
                0.75,
                1,
                Duration::from_secs(2),
                "30m".to_string(),
                mode,
            )
            .expect("build judge")
            // The rules would settle this candidate outright; this test is
            // about what happens when the *model* is unreachable.
            .with_rules(false)
        };

        let lenient = build(FallbackMode::Levenshtein)
            .judge_detailed(submission())
            .await
            .expect("a judge outage must not be an error");
        assert!(
            lenient.error.is_some(),
            "the outage is recorded, not hidden"
        );
        assert!(lenient.label.is_none(), "no verdict was actually reached");
        assert!(
            lenient.score > 0.75,
            "the Levenshtein fallback still scores this obvious match: {}",
            lenient.score,
        );

        let strict = build(FallbackMode::Reject)
            .judge_detailed(submission())
            .await
            .expect("judge");
        assert_eq!(strict.score, 0.0);
        assert!(strict.error.is_some());
    }

    /// Talks to a real ollama, so it is ignored by default. Run it after
    /// changing the prompt or the model tag — it is the only check that the
    /// serving contract still matches what the model was trained on:
    ///
    ///   SMOL_JUDGE_URL=http://127.0.0.1:11434 cargo test smol_judge_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires a running ollama serving the judge model"]
    async fn smol_judge_live_labels_a_remix_as_the_wrong_version() {
        use super::SmolJudge;
        use crate::internals::judge::judge_manager::Judge;
        use crate::internals::search::search_manager::{
            DownloadableFile, JudgeSubmission, SearchItem,
        };

        let judge = SmolJudge::new(
            std::env::var("SMOL_JUDGE_URL").unwrap_or_else(|_| SmolJudge::default_url()),
            SmolJudge::default_model(),
            0.75,
        )
        .expect("build judge");
        let ask = async |filename: &str| {
            judge
                .judge_detailed(JudgeSubmission::new(
                    SearchItem::new(
                        "t".to_string(),
                        "Only Human".to_string(),
                        "Only Human".to_string(),
                        "KH".to_string(),
                    ),
                    DownloadableFile {
                        filename: filename.to_string(),
                        username: "peer".to_string(),
                        size: 1024,
                    },
                ))
                .await
                .expect("judge")
        };

        let original = ask("Music\\KH\\2019 KH - Only Human\\01 - Only Human.flac").await;
        let remix = ask("Music\\KH\\Only Human (Skream Remix).mp3").await;
        let unrelated = ask("Massive Attack - Unfinished Sympathy.flac").await;
        println!("original={original:?}\nremix={remix:?}\nunrelated={unrelated:?}");

        assert_eq!(original.label.as_deref(), Some("exact"));
        assert!(original.score > 0.75, "the original must be downloadable");
        // The whole reason this judge exists: string distance cannot separate
        // these two, and accepting the remix is the failure it prevents.
        assert_ne!(remix.label.as_deref(), Some("exact"));
        assert!(remix.score < 0.75);
        assert!(unrelated.score < 0.75);
        assert!(original.error.is_none(), "ollama should be reachable");
    }

    #[test]
    fn only_exact_clears_the_download_threshold() {
        assert!(Label::Exact.score() > 0.75);
        assert!(Label::WrongVersion.score() < 0.75);
        assert!(Label::WrongArtist.score() < 0.75);
        assert_eq!(Label::Different.score(), 0.0);
    }
}
