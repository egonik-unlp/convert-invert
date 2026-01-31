use crate::internals::{
    context::context_manager::Track,
    search::search_manager::{DownloadableFile, JudgeSubmission, SearchItem},
};
use anyhow::Context;
use soulseek_rs::{Client, DownloadStatus, SearchResult};
use std::{collections::HashSet, path::PathBuf, str::FromStr, sync::Arc, time::Duration};
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinHandle,
};
use tracing::{Instrument, info_span, instrument};

fn is_audio_file(filename: String) -> bool {
    let lc = filename.to_lowercase();
    lc.ends_with(".mp3") || lc.ends_with(".flac") || lc.ends_with(".aiff")
}

pub struct DownloadableFiles(pub Vec<DownloadableFile>);
impl From<SearchResult> for DownloadableFiles {
    fn from(value: SearchResult) -> Self {
        let values = value
            .files
            .into_iter()
            .map(|file| {
                let filename = file.name;
                let username = file.username;
                let size = file.size;
                DownloadableFile {
                    filename,
                    username,
                    size,
                }
            })
            .collect();
        DownloadableFiles(values)
    }
}
impl From<DownloadableFiles> for Vec<DownloadableFile> {
    fn from(value: DownloadableFiles) -> Self {
        value.0
    }
}
pub struct DownloadManager {
    client: Arc<Client>,
    root_location: PathBuf,
    download_queue: mpsc::Receiver<JudgeSubmission>,
}

impl DownloadManager {
    pub fn new(
        client: Arc<Client>,
        root_location: PathBuf,
        download_queue: mpsc::Receiver<JudgeSubmission>,
    ) -> Self {
        DownloadManager {
            client,
            root_location,
            download_queue,
        }
    }
    #[instrument(name = "DownloadManager::run_channel", skip(self))]
    pub async fn run_channel(&mut self) -> anyhow::Result<()> {
        let mut downloaded_files = HashSet::new();
        let mut threads = vec![];
        let span = info_span!("download-blocking-thread-spawner");
        while let Some(song) = self.download_queue.recv().await {
            tracing::debug!("Started loop for {:?}", song.clone());
            let _othersong = song.clone();
            let client = Arc::clone(&self.client);
            let download_location = self.root_location.clone();
            if is_audio_file(song.query.filename.clone())
                && !has_been_downloaded(&song.track, &downloaded_files)
            {
                downloaded_files.insert(song.track.clone());
                let thread: JoinHandle<anyhow::Result<()>> = tokio::spawn(
                    async move {
                        download_track(song, download_location.clone(), client)
                            .await
                            .context("Downloading track")?;
                        Ok(())
                    }
                    .instrument(span.clone()),
                );
                threads.push(thread);
                tracing::info!("After download track in run");
            } else {
                tracing::info!(
                    "Rejected non song & already downloaded file = {}",
                    song.query.filename
                )
            }
            tracing::debug!("Download loop closed for = {:?}", _othersong);
        }
        for thread in threads {
            thread
                .await
                .context("Joining download thread")?
                .context("inner")?;
        }
        tracing::info!("Download thread shutting down");
        Ok(())
    }
    pub async fn run(self, track: JudgeSubmission) -> anyhow::Result<Track> {
        todo!()
    }
}

fn has_been_downloaded(track: &SearchItem, tracks: &HashSet<SearchItem>) -> bool {
    tracks.contains(track)
}

#[tracing::instrument(name = "DownloadManager::download_track", skip(song, path, client), fields(
    id = song.track.id,
    song_name = song.query.filename,
    user_name = song.query.username,
))]
async fn download_track(
    song: JudgeSubmission,
    path: PathBuf,
    client: Arc<Client>,
) -> anyhow::Result<()> {
    let sem = Arc::new(Semaphore::new(5));
    let song_path = PathBuf::from_str(&song.query.filename).context("Can't parse filename")?;
    let path = path.join(song_path.file_name().context("Cannot create file")?);
    let path_str = path.as_path().to_str().context("Non valid path")?;
    let semaphore = Arc::clone(&sem);
    let permit = semaphore.acquire().await.context("Getting permit")?;
    match client.download(
        song.query.filename.clone(),
        song.query.username,
        song.query.size,
        path_str.to_string(),
    ) {
        Ok(rec) => {
            let span = tracing::info_span!("download_thread_inner");
            let download_handle = tokio::task::spawn_blocking(move || {
                let mut notified_tracks = HashSet::new();
                span.in_scope(|| {
                    while let Ok(status) = rec.recv_timeout(Duration::from_secs(60)) {
                        if let DownloadStatus::Queued = status
                            && !notified_tracks.contains(&song.query.filename)
                        {
                            let track = song.query.filename.clone();
                            notified_tracks.insert(track.clone());
                            tracing::info!("Encolado {}", track);
                        }
                        if let DownloadStatus::Completed = status {
                            tracing::info!("completado {}", song.query.filename.clone());
                            break;
                        }
                        if let DownloadStatus::Failed | DownloadStatus::TimedOut = status {
                            tracing::info!("fallado {}", song.query.filename.clone());
                            break;
                        }
                        if let DownloadStatus::InProgress {
                            bytes_downloaded,
                            total_bytes,
                            speed_bytes_per_sec,
                        } = status
                            && bytes_downloaded % 4 == 0
                        {
                            tracing::info!(
                                "Downloaded {} of {} at {} bytes/s for {} ",
                                bytes_downloaded,
                                total_bytes,
                                speed_bytes_per_sec,
                                song.query.filename
                            )
                        }
                    }
                    tracing::info!("Reached ending of blocking thread")
                });
            });
            download_handle.await.context("Download thread down")?;
            tracing::info!("Ending download loop");
            drop(permit);
        }
        Err(error) => tracing::error!("Error en descarga {}", error),
    }
    Ok(())
}
