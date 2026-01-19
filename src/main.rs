use std::{
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::Context;
use soulseek_rs::{Client, ClientSettings};
use tracing::instrument;

use convert_invert::internals::{
    context::context_manager::{self, Managers},
    download::download_manager::DownloadManager,
    judge::judge_manager::{self, Judge, JudgeManager, Levenshtein},
    query::query_manager::QueryManager,
    search::search_manager::SearchManager,
    utils::{config::config_manager::Config, trace},
};

#[instrument]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::try_from_env().context("Cannot read env vars for config")?;

    tracing::info!(config = ?config, "read config");
    trace::otel_trace::init_tracing_with_otel("convert-invert".to_string(), config.run_id)
        .context("Tracing")?;

    let client_settings = ClientSettings {
        username: config.user_name.clone(),
        password: config.user_password,
        listen_port: config.listen_port,
        ..Default::default()
    };

    let download_path =
        PathBuf::from_str("/home/gonik/Music/falopa_vibe").context("Acquiring download dir")?;

    let (search_manager, download_manager, judge_manager, query_manager) = {
        let mut client = Client::with_settings(client_settings);
        client.connect();
        client.login().context("client login")?;
        let client = Arc::new(client);

        tracing::info!(user_name = config.user_name, "Logged in");

        let search_manager = SearchManager::new_context(client.clone());
        let lev_score = config.judge_score_levenshtein.unwrap_or(0.75);
        let lev_judge: Arc<dyn Judge> = Arc::new(Levenshtein::new(lev_score));
        let judge_manager = JudgeManager::new_context(lev_judge);

        let download_manager = DownloadManager::new_context(client.clone(), download_path);

        let query_manager = QueryManager::new_context("");
        (
            search_manager,
            download_manager,
            judge_manager,
            query_manager,
        )
    };

    let (track_tx, track_rx) = tokio::sync::mpsc::channel(2000);
    let managers = Managers {
        download_manager,
        search_manager,
        query_manager,
        judge_manager,
        search_timeout_secs: config.search_timeout_secs.into(),
        max_search_timeout_secs: config.max_search_timeout_secs,
        max_search_retries: config.max_search_retries,
        max_judge_retries: config.max_judge_retries,
        max_download_retries: config.max_download_retries,
        base_backoff_secs: config.base_backoff_secs,
        max_backoff_secs: config.max_backoff_secs,
        max_concurrent_searches: config.max_concurrent_searches,
        max_concurrent_judges: config.max_concurrent_judges,
        max_concurrent_downloads: config.max_concurrent_downloads,
    };
    let initial = managers.query_manager.run().await.context("Query")?;
    let pending = Arc::new(AtomicUsize::new(0));
    pending.fetch_add(1, Ordering::SeqCst);
    track_tx.send(initial).await.context("Seeding context")?;
    context_manager::manager(track_rx, track_tx, managers, pending)
        .await
        .context("Context manager")?;

    trace::otel_trace::shutdown_otel();

    Ok(())
}
