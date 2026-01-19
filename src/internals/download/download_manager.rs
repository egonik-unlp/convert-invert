use crate::internals::{
    context::context_manager::{
        DownloadFailure, DownloadFailureInfo, DownloadRequest, DownloadedFile, Track,
    },
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
    pub fn new_context(client: Arc<Client>, root_location: PathBuf) -> Self {
        let (_download_tx, download_queue) = mpsc::channel(1);
        DownloadManager {
            client,
            root_location,
            download_queue,
        }
    }
    pub fn context_clone(&self) -> Self {
        DownloadManager::new_context(self.client.clone(), self.root_location.clone())
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
                        let outcome = download_track(song, download_location.clone(), client)
                            .await
                            .context("Downloading track")?;
                        if !matches!(outcome, DownloadOutcome::Completed) {
                            tracing::warn!(outcome = ?outcome, "Download failed");
                        }
                        Ok(())
                    }
                    .instrument(span.clone()),
                );
                threads.push(thread);
                tracing::debug!("After download track in run");
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
    pub async fn run(&self, request: DownloadRequest) -> anyhow::Result<Track> {
        let path = download_path(&self.root_location, &request.submission)?;
        let outcome = download_track(
            request.submission.clone(),
            self.root_location.clone(),
            Arc::clone(&self.client),
        )
        .await
        .context("Downloading track")?;
        Ok(match outcome {
            DownloadOutcome::Completed => Track::File(DownloadedFile {
                submission: request.submission,
                path,
            }),
            DownloadOutcome::Failed => Track::DownloadFailed(DownloadFailureInfo {
                request: request.request,
                submission: request.submission,
                failure: DownloadFailure::Failed,
            }),
            DownloadOutcome::TimedOut => Track::DownloadFailed(DownloadFailureInfo {
                request: request.request,
                submission: request.submission,
                failure: DownloadFailure::TimedOut,
            }),
        })
    }
}

fn has_been_downloaded(track: &SearchItem, tracks: &HashSet<SearchItem>) -> bool {
    tracks.contains(track)
}

#[derive(Debug)]
enum DownloadOutcome {
    Completed,
    Failed,
    TimedOut,
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
) -> anyhow::Result<DownloadOutcome> {
    let sem = Arc::new(Semaphore::new(1));
    let song_path = PathBuf::from_str(&song.query.filename).context("Can't parse filename")?;
    let path = path.join(song_path.file_name().context("Cannot create file")?);
    let path_str = path.as_path().to_str().context("Non valid path")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Creating download directory")?;
    }
    tracing::info!(
        path = %path_str,
        file = %song.query.filename,
        user = %song.query.username,
        "Starting download"
    );
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
                let outcome = span.in_scope(|| {
                    let mut outcome = DownloadOutcome::TimedOut;
                    loop {
                        match rec.recv_timeout(Duration::from_secs(60)) {
                            Ok(status) => {
                                if let DownloadStatus::Queued = status
                                    && !notified_tracks.contains(&song.query.filename)
                                {
                                    let track = song.query.filename.clone();
                                    notified_tracks.insert(track.clone());
                                    tracing::info!("Encolado {}", track);
                                }
                                if let DownloadStatus::Completed = status {
                                    tracing::info!("completado {}", song.query.filename.clone());
                                    outcome = DownloadOutcome::Completed;
                                    break;
                                }
                                if let DownloadStatus::Failed | DownloadStatus::TimedOut = status {
                                    tracing::info!("fallado {}", song.query.filename.clone());
                                    outcome = match status {
                                        DownloadStatus::Failed => DownloadOutcome::Failed,
                                        DownloadStatus::TimedOut => DownloadOutcome::TimedOut,
                                        _ => DownloadOutcome::Failed,
                                    };
                                    break;
                                }
                                if let DownloadStatus::InProgress {
                                    bytes_downloaded,
                                    total_bytes,
                                    speed_bytes_per_sec,
                                } = status
                                    && bytes_downloaded % 4 == 0
                                {
                                    tracing::debug!(
                                        "Downloaded {} of {} at {} bytes/s for {} ",
                                        bytes_downloaded,
                                        total_bytes,
                                        speed_bytes_per_sec,
                                        song.query.filename
                                    )
                                }
                            }
                            Err(err) => {
                                tracing::warn!(error = ?err, "Download status timed out");
                                outcome = DownloadOutcome::TimedOut;
                                break;
                            }
                        }
                    }
                    tracing::info!("Reached ending of blocking thread");
                    outcome
                });
                outcome
            });
            let outcome = download_handle.await.context("Download thread down")?;
            tracing::info!(outcome = ?outcome, "Ending download loop");
            drop(permit);
            Ok(outcome)
        }
        Err(error) => {
            tracing::error!("Error en descarga {}", error);
            Ok(DownloadOutcome::Failed)
        }
    }
}

fn download_path(root: &PathBuf, track: &JudgeSubmission) -> anyhow::Result<PathBuf> {
    let song_path = PathBuf::from_str(&track.query.filename).context("Can't parse filename")?;
    Ok(root.join(song_path.file_name().context("Cannot create file")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_path_uses_basename() {
        let root = PathBuf::from("/tmp/downloads");
        let submission = JudgeSubmission {
            track: SearchItem {
                id: 1,
                track: "Track".to_string(),
                album: "Album".to_string(),
                artist: "Artist".to_string(),
            },
            query: DownloadableFile {
                filename: "folder/file.mp3".to_string(),
                username: "user".to_string(),
                size: 1,
            },
        };
        let path = download_path(&root, &submission).expect("path");
        assert_eq!(path, root.join("file.mp3"));
    }
}
