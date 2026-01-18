use std::{path::PathBuf, str::FromStr, sync::Arc};

use anyhow::Context;
use soulseek_rs::{Client, ClientSettings};
use tracing::{Instrument, info_span, instrument};

use convert_invert::internals::{
    download::download_manager::DownloadManager,
    judge::judge_manager::{JudgeManager, Levenshtein},
    query::query_manager::QueryManager,
    search::search_manager::SearchManager,
    utils::{config::config_manager::Config, trace},
};

#[instrument]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config = Config::try_from_env().context("Cannot read env vars for config")?;
    let attempt_num: usize = match std::env::args().nth(1) {
        Some(value) => value.parse().unwrap(),
        None => 1usize,
    };
    config.run_id = format!("{}_attempt_{}", config.run_id, attempt_num);
    println!("run_id: {}", config.run_id);
    tracing::info!(config = ?config, "read config");
    trace::otel_trace::init_tracing_with_otel("convert_invert".to_string(), config.run_id)
        .context("Tracing")?;

    let client_settings = ClientSettings {
        username: config.user_name.clone(),
        password: config.user_password,
        listen_port: config.listen_port,
        ..Default::default()
    };

    let download_path =
        PathBuf::from_str("/home/gonik/Music/randomRepeated").context("Acquiring download dir")?;

    let (search_manager, mut download_manager, judge_manager, query_manager) = {
        let mut client = Client::with_settings(client_settings);
        client.connect();
        client.login().context("client login")?;
        let client = Arc::new(client);

        tracing::info!(user_name = config.user_name, "Logged in");

        let (data_tx, data_rx) = tokio::sync::mpsc::channel(2000);
        let (results_tx, results_rx) = tokio::sync::mpsc::channel(2000);
        let (download_tx, download_rx) = tokio::sync::mpsc::channel(2000);

        let search_manager = SearchManager::new(client.clone(), data_rx, results_tx);

        let lev_score = config.judge_score_levenshtein.unwrap_or(0.75);
        let lev_judge = Levenshtein::new(lev_score);
        let judge_manager = JudgeManager::new(results_rx, download_tx, Box::new(lev_judge));

        let download_manager = DownloadManager::new(client.clone(), download_path, download_rx);

        let query_manager = QueryManager::new("", data_tx);
        (
            search_manager,
            download_manager,
            judge_manager,
            query_manager,
        )
    };

    let search_span = info_span!("search_thread");
    let judge_span = info_span!("judge_thread");
    let download_span = info_span!("download_thread");
    let query_span = info_span!("query_thread");

    let search_thread =
        tokio::spawn(async move { search_manager.run_channel().await }.instrument(search_span));
    let judge_thread =
        tokio::spawn(async move { judge_manager.run_channel().await }.instrument(judge_span));
    let download_thread =
        tokio::spawn(async move { download_manager.run_channel().await }.instrument(download_span));
    let query_thread =
        tokio::spawn(async move { query_manager.run_channel().await }.instrument(query_span));

    query_thread
        .await
        .context("Query thread rejoin error")?
        .context("Error handling judge")?;
    download_thread
        .await
        .context("Download thread joining")?
        .context("inner")?;
    judge_thread
        .await
        .context("Judge thread rejoin error")?
        .context("Error handling judge")?;
    search_thread
        .await
        .context("Search thread joining")?
        .context("inner")?;

    trace::otel_trace::shutdown_otel();

    Ok(())
}
