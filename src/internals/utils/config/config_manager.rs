use std::env;

use anyhow::Context;
use tracing_subscriber::EnvFilter;

#[allow(dead_code)]
#[derive(Debug, Default, Clone)]
pub struct Config {
    pub run_id: String,
    pub log_level: EnvFilter,
    pub user_name: String,
    pub user_password: String,
    pub judge_score_levenshtein: Option<f32>,
    pub judge_score_llm: Option<f32>,
    pub listen_port: u32,
    pub search_timeout_secs: u8,
    pub max_search_timeout_secs: u64,
    pub max_search_retries: u8,
    pub max_judge_retries: u8,
    pub max_download_retries: u8,
    pub base_backoff_secs: u64,
    pub max_backoff_secs: u64,
    pub max_concurrent_searches: usize,
    pub max_concurrent_judges: usize,
    pub max_concurrent_downloads: usize,
}

impl Config {
    pub fn try_from_env() -> anyhow::Result<Self> {
        dotenvy::dotenv().context("Getting env vars")?;
        let run_id = env::var("RUN_ID").unwrap_or("default_run_name".to_string());
        let log_level: EnvFilter = env::var("LOG_LEVEL").unwrap_or("debug".to_string()).into();
        let user_name = env::var("USER_NAME").unwrap_or("default".to_string());
        let user_password = env::var("USER_NAME").unwrap_or("123456".to_string());
        let judge_score_levenshtein: Option<f32> = {
            let val = env::var("JUDGE_SCORE_LEVENSHTEIN").ok();
            val.map(|v| v.parse().expect("Cannot parse judge score levenshtein"))
        };
        let judge_score_llm: Option<f32> = {
            let val = env::var("JUDGE_SCORE_LLM").ok();
            val.map(|v| v.parse().expect("Cannot parse judge score llm"))
        };
        let listen_port: u32 = {
            let val = env::var("LISTEN_PORT").unwrap_or("3124".to_string());
            val.parse().context("cannot parse val")?
        };

        let search_timeout_secs: u8 = {
            let val = env::var("SEARCH_TIMEOUT_SECS").unwrap_or("10".to_string());
            val.parse().context("cannot parse val")?
        };
        let max_search_timeout_secs: u64 = {
            let val = env::var("MAX_SEARCH_TIMEOUT_SECS").unwrap_or("120".to_string());
            val.parse().context("cannot parse max search timeout")?
        };
        let max_search_retries: u8 = {
            let val = env::var("MAX_SEARCH_RETRIES").unwrap_or("2".to_string());
            val.parse().context("cannot parse max search retries")?
        };
        let max_judge_retries: u8 = {
            let val = env::var("MAX_JUDGE_RETRIES").unwrap_or("2".to_string());
            val.parse().context("cannot parse max judge retries")?
        };
        let max_download_retries: u8 = {
            let val = env::var("MAX_DOWNLOAD_RETRIES").unwrap_or("2".to_string());
            val.parse().context("cannot parse max download retries")?
        };
        let base_backoff_secs: u64 = {
            let val = env::var("BASE_BACKOFF_SECS").unwrap_or("5".to_string());
            val.parse().context("cannot parse base backoff secs")?
        };
        let max_backoff_secs: u64 = {
            let val = env::var("MAX_BACKOFF_SECS").unwrap_or("60".to_string());
            val.parse().context("cannot parse max backoff secs")?
        };
        let max_concurrent_searches: usize = {
            let val = env::var("MAX_CONCURRENT_SEARCHES").unwrap_or("4".to_string());
            val.parse().context("cannot parse max concurrent searches")?
        };
        let max_concurrent_judges: usize = {
            let val = env::var("MAX_CONCURRENT_JUDGES").unwrap_or("8".to_string());
            val.parse().context("cannot parse max concurrent judges")?
        };
        let max_concurrent_downloads: usize = {
            let val = env::var("MAX_CONCURRENT_DOWNLOADS").unwrap_or("2".to_string());
            val.parse().context("cannot parse max concurrent downloads")?
        };
        Ok(Config {
            run_id,
            log_level,
            user_name,
            user_password,
            judge_score_levenshtein,
            judge_score_llm,
            listen_port,
            search_timeout_secs,
            max_search_timeout_secs,
            max_search_retries,
            max_judge_retries,
            max_download_retries,
            base_backoff_secs,
            max_backoff_secs,
            max_concurrent_searches,
            max_concurrent_judges,
            max_concurrent_downloads,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        log_level: EnvFilter,
        user_name: String,
        user_password: String,
        judge_score_levenshtein: Option<f32>,
        judge_score_llm: Option<f32>,
        listen_port: u32,
        search_timeout_secs: u8,
        max_search_timeout_secs: u64,
        max_search_retries: u8,
        max_judge_retries: u8,
        max_download_retries: u8,
        base_backoff_secs: u64,
        max_backoff_secs: u64,
        max_concurrent_searches: usize,
        max_concurrent_judges: usize,
        max_concurrent_downloads: usize,
        run_id: String,
    ) -> Self {
        Config {
            run_id,
            log_level,
            user_name,
            user_password,
            judge_score_levenshtein,
            judge_score_llm,
            listen_port,
            search_timeout_secs,
            max_search_timeout_secs,
            max_search_retries,
            max_judge_retries,
            max_download_retries,
            base_backoff_secs,
            max_backoff_secs,
            max_concurrent_searches,
            max_concurrent_judges,
            max_concurrent_downloads,
        }
    }
}
