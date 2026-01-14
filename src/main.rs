use std::{path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use anyhow::Context;
use soulseek_rs::{Client, ClientSettings};
use tokio::{task::JoinHandle, time::sleep};
use tracing::{Instrument, info_span};

use convert_invert::internals::{
    download::download_manager::DownloadManager,
    judge::judge_manager::{JudgeManager, Levenshtein},
    parsing::deserialize,
    search::search_manager::{SearchItem, SearchManager},
    utils::trace,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    trace::otel_trace::init_tracing_with_otel(
        "convert_invert".to_string(),
        "drop-permit-download".to_string(),
    )
    .context("Tracing")?;
    let data_string = include_str!("./internals/parsing/sample.json");
    let data: deserialize::Playlist = serde_json::from_str(data_string).unwrap();
    let queries: Vec<SearchItem> = data.into();
    let username = "gonik1994";
    let client_settings = ClientSettings {
        username: username.to_string(),
        password: "0112358".to_string(),
        listen_port: 3513,
        ..Default::default()
    };

    let download_path =
        PathBuf::from_str("/home/gonik/Music/quinta_falopa").context("Acquiring download dir")?;
    let (search_manager, mut download_manager, judge_manager, data_tx) = {
        let mut client = Client::with_settings(client_settings);
        client.connect();
        client.login().context("client login")?;
        println!("logged in with client: {}", username);
        let client = Arc::new(client);
        let (data_tx, data_rx) = tokio::sync::mpsc::channel(2000);
        let (results_tx, results_rx) = tokio::sync::mpsc::channel(2000);
        let (download_tx, download_rx) = tokio::sync::mpsc::channel(2000);
        let search_manager = SearchManager::new(client.clone(), data_rx, results_tx);

        let lev_judge = Levenshtein::new(0.75);
        let judge_manager = JudgeManager::new(results_rx, download_tx, Box::new(lev_judge));
        let download_manager = DownloadManager::new(client.clone(), download_path, download_rx);
        (search_manager, download_manager, judge_manager, data_tx)
    };

    let search_span = info_span!("search_thread");
    let judge_span = info_span!("judge_thread");
    let download_span = info_span!("download_thread");
    let search_thread =
        tokio::spawn(async move { search_manager.run().await }.instrument(search_span));
    let judge_thread =
        tokio::spawn(async move { judge_manager.run().await }.instrument(judge_span));
    let download_thread =
        tokio::spawn(async move { download_manager.run().await }.instrument(download_span));
    let query_thread: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
        let start_time = chrono::Local::now();
        for (n, song) in queries.into_iter().skip(4).enumerate() {
            data_tx
                .send(song.clone())
                .await
                .context("Sending query song")?;
            let elapsed = (chrono::Local::now() - start_time).as_seconds_f32();
            tracing::warn!("File Sent: {:?}\nHTP = {}", song, elapsed);
            if n % 10 == 0 && n > 1 {
                sleep(Duration::from_secs(200)).await;
            }
        }
        Ok(())
    });

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
