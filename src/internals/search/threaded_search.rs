#![allow(unused_labels)]

use std::{
    fs::OpenOptions,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{self, Duration},
};

use serde::{Deserialize, Serialize};
use soulseek_rs::{DownloadStatus, SearchResult};
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::internals::parsing::deserialize::Playlist;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SearchItem {
    pub track: String,
    pub album: String,
    pub artist: String,
    pub found: bool,
}
impl From<Playlist> for Vec<SearchItem> {
    fn from(value: Playlist) -> Vec<SearchItem> {
        let tracks = value.tracks.unwrap().items.unwrap();
        tracks
            .into_iter()
            .map(|tr| SearchItem {
                track: tr.clone().track.unwrap().name.unwrap(),
                artist: tr
                    .track
                    .clone()
                    .unwrap()
                    .artists
                    .unwrap()
                    .first()
                    .unwrap()
                    .name
                    .clone()
                    .unwrap()
                    .clone(),
                album: tr.track.unwrap().album.unwrap().name.unwrap(),
                found: false,
            })
            .collect()
    }
}
#[derive(Debug, Clone)]
pub enum Status {
    Done(String),
    InSearch,
    Downloading(String),
}
pub struct SearchManager {
    pub client: Arc<soulseek_rs::Client>,
    pub data_rx: mpsc::Receiver<SearchItem>,
    pub status_tx: mpsc::Sender<Status>,
    pub results_tx: mpsc::Sender<SearchResult>,
    pub handles: Vec<tokio::task::JoinHandle<()>>,
}

impl SearchManager {
    pub fn new(
        client: Arc<soulseek_rs::Client>,
        data_rx: tokio::sync::mpsc::Receiver<SearchItem>,
        status_tx: tokio::sync::mpsc::Sender<Status>,
        results_tx: tokio::sync::mpsc::Sender<SearchResult>,
    ) -> Self {
        SearchManager {
            client,
            data_rx,
            status_tx,
            results_tx,
            handles: Vec::new(),
        }
    }

    pub async fn run(&mut self) {
        while let Some(search_item) = self.data_rx.recv().await {
            let client = self.client.clone();
            let status_tx = self.status_tx.clone();
            let results_tx = self.results_tx.clone();
            let handle = tokio::spawn(async move {
                track_search_task(client, search_item, status_tx, results_tx).await;
            });
            self.handles.push(handle);
        }

        for handle in self.handles.drain(..) {
            handle.await.unwrap();
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct OwnSearchResult {
    filenames: Vec<String>,
}
impl From<SearchResult> for OwnSearchResult {
    fn from(value: SearchResult) -> Self {
        let filenames = value.files.into_iter().map(|f| f.name).collect();
        OwnSearchResult { filenames }
    }
}
#[derive(Debug, Deserialize, Serialize)]
pub struct DumpData {
    results: Vec<OwnSearchResult>,
    query: SearchItem,
}

pub async fn track_search_task(
    client: Arc<soulseek_rs::Client>,
    data: SearchItem,
    status_tx: mpsc::Sender<Status>,
    results_tx: mpsc::Sender<SearchResult>,
) {
    let query_string = format!("{} - {}", data.track.as_str(), data.artist);
    println!("Searching for: {}", query_string);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut find_history: Vec<soulseek_rs::SearchResult> = Vec::new();

    let search_thread = {
        let search_client = client.clone();
        let query_string_search = query_string.clone();
        let cancel_search = cancel.clone();
        tokio::task::spawn_blocking(move || {
            search_client.search_with_cancel(
                query_string_search.as_str(),
                Duration::from_secs(1000), // Reduced duration for faster exit
                Some(cancel_search),
            )
        })
    };

    let mut count = 0;
    let mut extensions = 0;
    status_tx.send(Status::InSearch).await.unwrap();
    'main: loop {
        if find_history.len() > 5 {
            cancel.store(true, Ordering::Relaxed);
            println!("Not searching anymore for {} because it ends", query_string);
            break;
        };
        sleep(Duration::from_secs(10)).await;
        let results = client.get_search_results(&query_string);
        if !results.is_empty() {
            for result in results.clone() {
                println!("Enviando results");
                if !result.files.is_empty() {
                    match results_tx.send(result.to_owned()).await {
                        Ok(()) => println!("Result correctly sent: {:?}", result),
                        Err(err) => {
                            println!("Error in sending:\nObject:{:?}\nError{:?}", result, err)
                        }
                    };
                }
            }
            find_history.extend(results);
            extensions += 1;
            dump_data_logging(data.clone(), find_history.clone());
        } else {
            count += 1;
        }
        if count > 1 {
            println!(
                "Exited because one consecutive empty results for: {}",
                query_string
            );
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            status_tx.send(Status::Done(query_string)).await.unwrap();
            break 'main;
        }
    }
    search_thread.await.unwrap().unwrap();
}

fn dump_data_logging(data: SearchItem, find_history: Vec<soulseek_rs::SearchResult>) {
    let query = data.clone();
    let filename_dump = format!("dump_{}_{}.json", data.track.as_str(), data.artist);
    let mut file_dump = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(filename_dump.clone())
        .unwrap();
    let mut filenames_dump = OpenOptions::new()
        .create(true)
        .append(true)
        .open("dump_writelens.txt")
        .unwrap();
    let results: Vec<OwnSearchResult> =
        find_history.clone().into_iter().map(|f| f.into()).collect();
    let writefiles_string = format!(
        "writelen = {}, stamp = {:?}, file = {}\n",
        results.len(),
        time::Instant::now(),
        filename_dump
    );
    filenames_dump
        .write_all(writefiles_string.as_bytes())
        .unwrap();
    let data_dump = DumpData { results, query };
    let dump_string = serde_json::to_string(&data_dump).unwrap();
    file_dump.write_all(dump_string.as_bytes()).unwrap();
}
