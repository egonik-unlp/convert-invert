use crate::internals::search::search_manager::{DownloadableFile, JudgeSubmission, SearchItem};
use anyhow::Context;
use soulseek_rs::{Client, DownloadStatus, SearchResult};
use std::{collections::HashSet, path::PathBuf, str::FromStr, sync::Arc, time::Duration};
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinHandle,
};
use tracing::instrument;

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
    #[instrument(name = "DownloadManager::run", skip(self))]
    pub async fn run(&mut self) -> anyhow::Result<()> {
        let mut downloaded_files = HashSet::new();
        let mut threads = vec![];
        while let Some(song) = self.download_queue.recv().await {
            tracing::debug!("Started loop for {:?}", song.clone());
            let _othersong = song.clone();
            let client = Arc::clone(&self.client);
            let download_location = self.root_location.clone();
            if is_audio_file(song.query.filename.clone())
                && !has_been_downloaded(&song.track, &downloaded_files)
            {
                downloaded_files.insert(song.track.clone());
                let thread: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
                    download_track(song.query, download_location.clone(), client)
                        .await
                        .context("Downloading track")?;
                    Ok(())
                });
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
}

fn has_been_downloaded(track: &SearchItem, tracks: &HashSet<SearchItem>) -> bool {
    tracks.contains(track)
}

#[tracing::instrument(name = "DownloadManager::download_track", skip(song, path, client), fields(
    song_name = song.filename,
    user_name = song.username,
))]
async fn download_track(
    song: DownloadableFile,
    path: PathBuf,
    client: Arc<Client>,
) -> anyhow::Result<()> {
    let sem = Arc::new(Semaphore::new(1));
    let song_path = PathBuf::from_str(&song.filename).context("Can't parse filename")?;
    let path = path.join(song_path.file_name().context("Cannot create file")?);
    let path_str = path.as_path().to_str().context("Non valid path")?;
    let semaphore = Arc::clone(&sem);
    let permit = semaphore.acquire().await.context("Getting permit")?;
    match client.download(
        song.filename.clone(),
        song.username,
        song.size,
        path_str.to_string(),
    ) {
        Ok(rec) => {
            let span = tracing::info_span!("download_thread_inner");
            let download_handle = tokio::task::spawn_blocking(move || {
                let mut notified_tracks = HashSet::new();
                span.in_scope(|| {
                    while let Ok(status) = rec.recv_timeout(Duration::from_secs(60)) {
                        if let DownloadStatus::Queued = status
                            && !notified_tracks.contains(&song.filename)
                        {
                            let track = song.filename.clone();
                            notified_tracks.insert(track.clone());
                            tracing::info!("Encolado {}", track);
                        }
                        if let DownloadStatus::Completed = status {
                            tracing::info!("completado {}", song.filename.clone());
                            break;
                        }
                        if let DownloadStatus::Failed | DownloadStatus::TimedOut = status {
                            tracing::info!("fallado {}", song.filename.clone());
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
                                song.filename
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
