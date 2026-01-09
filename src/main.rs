use convert_invert::internals::{
    parsing::deserialize,
    search::threaded_search::{OwnSearchResult, SearchItem, SearchManager, Status},
};
use serde::{Deserialize, Serialize};
use soulseek_rs::{Client, ClientSettings, SearchResult};
use std::{fs::OpenOptions, io::Write, sync::Arc};
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_string = include_str!("./internals/parsing/sample.json");
    let data: deserialize::Playlist = serde_json::from_str(data_string).unwrap();
    let queries: Vec<SearchItem> = data.into();
    let client_settings = ClientSettings {
        username: "egonik948".to_string(),
        password: "0112358".to_string(),
        listen_port: 3217,
        ..Default::default()
    };
    let mut client = Client::with_settings(client_settings);
    client.connect();
    client.login()?;
    println!("logged in");

    let client = Arc::new(client);

    let (data_tx, data_rx) = tokio::sync::mpsc::channel(100);
    let (status_tx, mut status_rx) = tokio::sync::mpsc::channel(100);
    let (results_tx, mut results_rx) = tokio::sync::mpsc::channel(100);

    let download_queue: Arc<Mutex<Vec<ResponseFormat>>> = Arc::new(Mutex::new(vec![]));
    let mut search_manager = SearchManager::new(client.clone(), data_rx, status_tx, results_tx);

    // Stream status updates live on a dedicated thread
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

    for song in queries.into_iter().take(3) {
        data_tx.send(song).await.unwrap();
    }
    // We need to close the channel so the `run` method can finish
    drop(data_tx);
    let judge_thread = tokio::spawn(async move {
        println!("Runing judge_thread\n\n\n");
        loop {
            println!("Awaiting result from search...");
            match results_rx.recv().await {
                Some(msg) => {
                    println!("\n\nRUNNING IN THREAD. CHECKING RES\n\nData:{:?}", msg);
                    let response = check_track(msg).await.unwrap();
                    println!("La responzetta es: {:?}", response);
                    let queue = Arc::clone(&download_queue);
                    let mut val = queue.lock().await;
                    (*val).push(response.clone());
                    write_file(&response);
                }
                None => {
                    println!("Channel closed. Judge thread exiting.");
                    break;
                }
            }
        }
    });
    search_thread.await.unwrap();
    status_thread.await.unwrap();
    judge_thread.await.unwrap();
    Ok(())
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ResponseFormat {
    score: Option<f32>,
    query_song: Option<String>,
    filename: Option<String>,
}

async fn check_track(track: SearchResult) -> anyhow::Result<ResponseFormat> {
    println!("Running checking functionality");
    let client = reqwest::Client::new();
    let myown: OwnSearchResult = track.into();
    let str_val = serde_json::to_string(&myown).unwrap();
    println!("\n\n\n\n\nAbout to send request\n\n\n\n\n");
    let res = client
        .post("http://localhost:6111/score")
        .header("Content-Type", "application/json")
        .body(str_val)
        .send()
        .await
        .unwrap();
    let text = res.text().await.unwrap();
    let response: ResponseFormat = serde_json::from_str(&text).unwrap();
    Ok(response)
}

fn write_file(response: &ResponseFormat) {
    let response_text = serde_json::to_string(response).unwrap();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open("dumpito.json")
        .unwrap();
    file.write_all(response_text.as_bytes()).unwrap();
}
