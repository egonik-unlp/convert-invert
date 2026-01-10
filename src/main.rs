use anyhow::Context;
use convert_invert::internals::download::download_manager::DownloadManager;
use convert_invert::internals::judge::judge_manager::{JudgeManager, LocalLLM};
use convert_invert::internals::{
    parsing::deserialize,
    search::search_manager::{SearchItem, SearchManager, Status},
};
use soulseek_rs::{Client, ClientSettings};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tracing_subscriber::layer::SubscriberExt;

fn init_tracing() {
    let subscriber = tracing_subscriber::fmt()
        // Use a more compact, abbreviated log format
        .compact()
        // Display source code file paths
        .with_file(true)
        // Display source code line numbers
        .with_line_number(true)
        .with_thread_ids(true)
        // Don't display the event's target (module path)
        .with_target(true)
        // Build the subscriber
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    // console_subscriber::init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let data_string = include_str!("./internals/parsing/sample.json");
    let data: deserialize::Playlist = serde_json::from_str(data_string).unwrap();
    let queries: Vec<SearchItem> = data.into();
    let client_settings = ClientSettings {
        username: "gongon1993".to_string(),
        password: "0112358".to_string(),
        listen_port: 3217,
        ..Default::default()
    };
    let mut client = Client::with_settings(client_settings);
    client.connect();
    client.login().context("client login")?;
    println!("logged in");

    let download_path = PathBuf::from_str("../downloads/").context("Acquiring download dir")?;
    let client = Arc::new(client);

    let (data_tx, data_rx) = tokio::sync::mpsc::channel(100);
    let (status_tx, mut status_rx) = tokio::sync::mpsc::channel(100);
    let (results_tx, results_rx) = tokio::sync::mpsc::channel(100);
    let (download_tx, download_rx) = tokio::sync::mpsc::channel(100);
    let mut search_manager =
        SearchManager::new(client.clone(), data_rx, status_tx.clone(), results_tx);
    let llm_judge = LocalLLM::new("http:localhost".to_string(), 6111, 0.75);
    let judge_manager = JudgeManager::new(results_rx, download_tx, Box::new(llm_judge));

    let mut download_manager =
        DownloadManager::new(client.clone(), download_path, status_tx, download_rx);

    let status_thread = tokio::spawn(async move {
        while let Some(msg) = status_rx.recv().await {
            match msg {
                Status::Downloading(file) => println!("Downloading: {}", file),
                Status::Done(file) => println!("Downloaded: {}", file),
                Status::InSearch => println!("Searching..."),
            }
        }
    });
    let search_thread = tokio::spawn(async move { search_manager.run().await });
    let judge_thread = tokio::spawn(async move { judge_manager.run2().await });
    let download_thread = tokio::spawn(async move { download_manager.run().await });
    for song in queries.into_iter().take(3) {
        data_tx.send(song).await.context("Sending query song")?;
    }
    drop(data_tx);
    search_thread
        .await
        .context("Joining search thread")?
        .context("inner search")?;
    status_thread.await.context("Joining status thread")?;

    judge_thread
        .await
        .context("Judge thread rejoin error")?
        .context("Error handling judge")?;
    download_thread
        .await
        .context("Download thread joining")?
        .context("inner")?;
    Ok(())
}
