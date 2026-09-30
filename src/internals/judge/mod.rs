//! Deciding whether a Soulseek result is the recording that was requested.
//!
//! One judge scores each candidate; [`judge_manager::JudgeManager`] compares
//! that score against [`judge_manager::JUDGE_THRESHOLD`] and routes the
//! candidate to download or rejection. `JUDGE_BACKEND` picks the judge:
//!
//! - [`judges::levenshtein`] — normalised string similarity. Cheap, and unable
//!   to tell a track from its remix, because as strings they barely differ.
//! - [`judges::smol`] — a fine-tuned SmolLM served by ollama, which answers with
//!   a label (`exact`, `wrong_version`, `wrong_artist`, `different`). Only
//!   `exact` clears the threshold.
//!
//! [`judges::relative_mi`] runs alongside whichever judge is active, recorded
//! but never acted on.
//!
//! Every verdict is captured as a [`judge_manager::Judgement`] — label, model,
//! prompt version, latency, raw reply, error — carried on the submission and
//! persisted to the `judgements` table, which is what the dashboard's
//! Judgements view reads.

pub mod judge_manager;
pub mod judges;
