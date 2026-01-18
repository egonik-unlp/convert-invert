#![allow(unused_labels)]

use crate::internals::parsing::deserialize::Playlist;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use soulseek_rs::SearchResult;
use std::{
    collections::HashSet,
    fmt::Display,
    fs::OpenOptions,
    hash::{DefaultHasher, Hash, Hasher},
    io::Write,
    sync::{Arc, atomic::AtomicBool},
    time::{self, Duration},
};
use tokio::{
    sync::{Semaphore, mpsc},
    time::sleep,
};
use tracing::{Instrument, info_span, instrument};

const TIMES_WITH_NO_NEW_FILES: usize = 3;

#[derive(Debug, Deserialize, Serialize, Clone, Hash, PartialEq, Eq)]
pub struct SearchItem {
    pub id: u64,
    pub track: String,
    pub album: String,
    pub artist: String,
}
impl SearchItem {
    fn new(track: String, album: String, artist: String) -> Self {
        let id = {
            let mut s = DefaultHasher::new();
            track.hash(&mut s);
            album.hash(&mut s);
            artist.hash(&mut s);
            s.finish()
        };
        SearchItem {
            id,
            track,
            album,
            artist,
        }
    }
}
impl From<Playlist> for Vec<SearchItem> {
    fn from(value: Playlist) -> Vec<SearchItem> {
        let tracks = value.tracks.unwrap().items.unwrap();
        tracks
            .into_iter()
            .map(|tr| {
                let track = tr.clone().track.unwrap().name.unwrap();
                let artist = tr
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
                    .clone();
                let album = tr.track.unwrap().album.unwrap().name.unwrap();
                SearchItem::new(track, album, artist)
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
    pub async fn run_channel(mut self) -> anyhow::Result<()> {
        let sem = Arc::new(Semaphore::new(2));
        let span = info_span!("search-task");
        while let Some(search_item) = self.data_rx.recv().await {
            tracing::debug!(
                track = search_item.clone().track,
                artist = search_item.clone().artist,
                "Received search request"
            );
            let client = self.client.clone();
            let results_tx = self.results_tx.clone();
            let item = search_item.clone();
            let semaphore = Arc::clone(&sem);
            let hand = tokio::spawn(
                async move {
                    let _permit = semaphore
                        .acquire()
                        .await
                        .context("Acquiring Semaphore permit")?;
                    track_search_task(client, item, results_tx)
                        .await
                        .context("Track search context")?;
                    Ok(())
                }
                .instrument(span.clone()),
            );
            self.handles.push(hand);
            tracing::info!( search_item = ?search_item.clone() ,"Closed loop for run search" );
        }

        tracing::info!("Exited while loop for run search");
        for handle in self.handles {
            handle
                .await
                .context("Search thread error")?
                .context("inner error")?
        }
        tracing::info!("Search thread shutting down");
        Ok(())
    }
    pub async fn run(self, track: SearchItem) -> anyhow::Result<()> {
        todo!()
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
        id = data.id,
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
    let span = info_span!("track_blocking");
    let search_thread = {
        let search_client = client.clone();
        let query_string_search = query_string.clone();
        let cancel_search = cancel.clone();
        tokio::task::spawn_blocking(move || {
            {
                search_client.search_with_cancel(
                    query_string_search.as_str(),
                    Duration::from_secs(100), // Reduced duration for faster exit
                    Some(cancel_search),
                )
            }
        })
    }
    .instrument(span);
    let mut previous_submissions = HashSet::new();
    let mut count = 0;
    let mut total_files_found = 0;
    tracing::info!(
        string_query = &query_string,
        cancel = ?cancel,
        "About to enter search loop",
    );
    #[allow(unused_parens)]
    'main: loop {
        tracing::info!("First line in result processing loop");
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
                        tracing::debug!(submission = ?submission.clone(), "Sent submission");
                        tracing::debug!(
                            cancel =?cancel,
                            "Submission received with cancel being:"
                        );
                        results_tx
                            .send(submission.clone())
                            .await
                            .context("Error sending search result")?;
                        previous_submissions
                            .insert((submission.query.filename, submission.query.username));
                    } else {
                        tracing::debug!(
                            submission = ?submission,
                            "Rejected entry because it already was submitted",
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
                times = TIMES_WITH_NO_NEW_FILES,
                query_string = query_string,
                "Exited because consecutive empty results",
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
