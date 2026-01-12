use anyhow::Context;
use convert_invert::internals::download::download_manager::DownloadManager;
use convert_invert::internals::judge::judge_manager::{JudgeManager, Levenshtein};
use convert_invert::internals::utils::trace;
use convert_invert::internals::{
    parsing::deserialize,
    search::search_manager::{SearchItem, SearchManager},
};
use soulseek_rs::{Client, ClientSettings};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::Level;
use tracing_subscriber::EnvFilter;

fn init_tracing() {
    let filter = EnvFilter::from_default_env().add_directive(Level::DEBUG.into());
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
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
    // trace::otel_trace::init_tracing_with_otel("convert-invert".to_string()).context("Logging")?;
    let data_string = include_str!("./internals/parsing/sample.json");
    let data: deserialize::Playlist = serde_json::from_str(data_string).unwrap();
    let queries: Vec<SearchItem> = data.into();
    let username = "egon1994";
    let client_settings = ClientSettings {
        username: username.to_string(),
        password: "0112358".to_string(),
        listen_port: 3217,
        ..Default::default()
    };

    let download_path =
        PathBuf::from_str("/home/gonik/Music/tercera_falopa").context("Acquiring download dir")?;
    let (mut search_manager, mut download_manager, judge_manager, data_tx) = {
        let mut client = Client::with_settings(client_settings);
        client.connect();
        client.login().context("client login")?;
        println!("logged in with client: {}", username);
        let client = Arc::new(client);
        let (data_tx, data_rx) = tokio::sync::mpsc::channel(100);
        let (results_tx, results_rx) = tokio::sync::mpsc::channel(100);
        let (download_tx, download_rx) = tokio::sync::mpsc::channel(100);
        let search_manager = SearchManager::new(client.clone(), data_rx, results_tx);

        let lev_judge = Levenshtein::new(0.75);
        let judge_manager = JudgeManager::new(results_rx, download_tx, Box::new(lev_judge));
        let download_manager = DownloadManager::new(client.clone(), download_path, download_rx);
        (search_manager, download_manager, judge_manager, data_tx)
    };

    let search_thread = tokio::spawn(async move { search_manager.run().await });
    let judge_thread = tokio::spawn(async move { judge_manager.run2().await });
    let download_thread = tokio::spawn(async move { download_manager.run().await });

    let start_time = chrono::Local::now();
    for (n, song) in queries.into_iter().skip(40).enumerate() {
        data_tx
            .send(song.clone())
            .await
            .context("Sending query song")?;
        let elapsed = (chrono::Local::now() - start_time).as_seconds_f32();
        tracing::warn!("File Sent: {:?}\nHTP = {}", song, elapsed);
        if n % 10 == 0 {
            sleep(Duration::from_secs(90)).await;
        }
    }

    judge_thread
        .await
        .context("Judge thread rejoin error")?
        .context("Error handling judge")?;
    download_thread
        .await
        .context("Download thread joining")?
        .context("inner")?;
    search_thread
        .await
        .context("Joining search thread")?
        .context("inner search")?;
    // trace::otel_trace::shutdown_otel();
    Ok(())
}
