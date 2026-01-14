#![allow(unused_labels)]

use crate::internals::parsing::deserialize::Playlist;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use soulseek_rs::SearchResult;
use std::{
    collections::HashSet,
    fmt::Display,
    fs::OpenOptions,
    io::Write,
    sync::{Arc, atomic::AtomicBool},
    time::{self, Duration},
};
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::instrument;

const TIMES_WITH_NO_NEW_FILES: usize = 1;

#[derive(Debug, Deserialize, Serialize, Clone, Hash, PartialEq, Eq)]
pub struct SearchItem {
    pub track: String,
    pub album: String,
    pub artist: String,
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
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DownloadableFile {
    pub filename: String,
    pub username: String,
    pub size: u64,
}
impl Display for SearchItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} - {} - {}", self.track, self.artist, self.album)
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JudgeSubmission {
    pub track: SearchItem,
    pub query: DownloadableFile,
}

pub struct SearchManager {
    pub client: Arc<soulseek_rs::Client>,
    pub data_rx: mpsc::Receiver<SearchItem>,
    pub results_tx: mpsc::Sender<JudgeSubmission>,
    pub handles: Vec<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl SearchManager {
    pub fn new(
        client: Arc<soulseek_rs::Client>,
        data_rx: tokio::sync::mpsc::Receiver<SearchItem>,
        results_tx: tokio::sync::mpsc::Sender<JudgeSubmission>,
    ) -> Self {
        SearchManager {
            client,
            data_rx,
            results_tx,
            handles: vec![],
        }
    }
    fn build_submissions(track: SearchItem, result: SearchResult) -> Vec<JudgeSubmission> {
        result
            .files
            .into_iter()
            .map(|f| JudgeSubmission {
                query: DownloadableFile {
                    filename: f.name,
                    size: f.size,
                    username: f.username,
                },
                track: track.clone(),
            })
            .collect()
    }
    #[instrument(skip(self))]
    pub async fn run(mut self) -> anyhow::Result<()> {
        let sem = Arc::new(Semaphore::new(2));
        while let Some(search_item) = self.data_rx.recv().await {
            tracing::debug!("Received search request for: {:?}", search_item.clone());
            let client = self.client.clone();
            let results_tx = self.results_tx.clone();
            // let name = search_item.clone().track;
            let item = search_item.clone();
            let semaphore = Arc::clone(&sem);
            let hand = tokio::spawn(async move {
                let _permit = semaphore
                    .acquire()
                    .await
                    .context("Acquiring Semaphore permit")?;
                track_search_task(client, item, results_tx)
                    .await
                    .context("Track search context")?;
                // .instrument(span),
                // );
                Ok(())
            });
            self.handles.push(hand);
            tracing::info!("Closed loop for run search");
        }

        tracing::info!("Exited while loop for run search");
        for handle in self.handles {
            handle
                .await
                .context("Search thread error")?
                .context("inner error")?
        }
        Ok(())
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

#[instrument(
    name = "track_search_task",
    skip(client, results_tx),
    fields(
        query = ?data.track,
    )
)]
pub async fn track_search_task(
    client: Arc<soulseek_rs::Client>,
    data: SearchItem,
    results_tx: mpsc::Sender<JudgeSubmission>,
) -> anyhow::Result<()> {
    let query_string = format!("{} - {}", data.track.as_str(), data.artist);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut find_history: Vec<soulseek_rs::SearchResult> = Vec::new();
    let search_thread = {
        let search_client = client.clone();
        let query_string_search = query_string.clone();
        let cancel_search = cancel.clone();
        tokio::task::spawn_blocking(move || {
            search_client.search_with_cancel(
                query_string_search.as_str(),
                Duration::from_secs(100), // Reduced duration for faster exit
                Some(cancel_search),
            )
        })
    };
    let mut previous_submissions = HashSet::new();
    let mut count = 0;
    let mut total_files_found = 0;
    tracing::info!(
        "About to enter loop for {}, cancel = {:?}",
        &query_string,
        cancel
    );
    #[allow(unused_parens)]
    'main: loop {
        tracing::info!("First line in loop");
        sleep(Duration::from_secs(10)).await;
        let results = client.get_search_results(&query_string);
        let results_count: usize = results.iter().map(|res| res.files.len()).sum();
        tracing::info!("In infinite loop for {}", &query_string);
        if !results.is_empty() && !total_files_found.eq(&results_count) {
            find_history.extend(results.clone());
            total_files_found += (results_count - total_files_found);
            for result in results {
                let submisssions = SearchManager::build_submissions(data.clone(), result);
                for submission in submisssions {
                    if !previous_submissions.contains(&(
                        submission.query.filename.clone(),
                        submission.query.username.clone(),
                    )) {
                        tracing::debug!("Sent submission: {:?}", submission.clone());
                        tracing::debug!("Submission received with cancel being: {:?}", cancel);
                        results_tx
                            .send(submission.clone())
                            .await
                            .context("Error sending search result")?;
                        previous_submissions
                            .insert((submission.query.filename, submission.query.username));
                    } else {
                        tracing::debug!(
                            "Rejected entry {:?} because it already was submitted",
                            submission
                        );
                    }
                }
            }
            count = 0;
            dump_data_logging(data.clone(), find_history.clone()).context("dumping")?;
        } else {
            count += 1;
        }
        if count > TIMES_WITH_NO_NEW_FILES {
            tracing::info!(
                "Exited because {} consecutive empty results for: {}",
                TIMES_WITH_NO_NEW_FILES,
                query_string
            );
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            break 'main;
        }
    }
    search_thread
        .await
        .unwrap()
        .context("Inner search thread issue")?;
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

fn dump_data_logging(
    data: SearchItem,
    find_history: Vec<soulseek_rs::SearchResult>,
) -> anyhow::Result<()> {
    let query = data.clone();
    let filename_dump = format!("dumps/dump_{}_{}.json", data.track.as_str(), data.artist);
    let mut file_dump = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(filename_dump.clone())
        .context("creating filedump")?;
    let mut filenames_dump = OpenOptions::new()
        .create(true)
        .append(true)
        .open("dump_writelens.txt")
        .context("creating writelens")?;
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
    let dump_string = serde_json::to_string(&data_dump).context("Deserialization issue")?;
    file_dump
        .write_all(dump_string.as_bytes())
        .context("writing in dump")?;
    Ok(())
}
