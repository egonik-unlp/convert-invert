use crate::internals::{
    context::context_manager::{DownloadedFile, RetryRequest, Track},
    search::search_manager::{DownloadableFile, JudgeSubmission},
};
use anyhow::Context;
use soulseek_rs::{Client, DownloadStatus, SearchResult};
use std::{path::PathBuf, str::FromStr, sync::Arc, time::Duration};
use tokio::task::JoinHandle;

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
}

impl DownloadManager {
    pub fn new(client: Arc<Client>, root_location: PathBuf) -> Self {
        DownloadManager {
            client,
            root_location,
        }
    }
    pub async fn run(&self, track: JudgeSubmission) -> anyhow::Result<Option<Track>> {
        let client = Arc::clone(&self.client);
        let download_location = self.root_location.clone();
        if is_audio_file(track.query.filename.clone()) {
            tracing::info!(track.query.filename, "send to download");
            let track = download_track(track, download_location.clone(), client)
                .await
                .context("Downloading track")?;
            Ok(Some(track))
        } else {
            tracing::info!(
                track.query.filename,
                "Rejected non song & already downloaded file",
            );
            Ok(None)
        }
    }
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
) -> anyhow::Result<Track> {
    let song_path = PathBuf::from_str(&song.query.filename).context("Can't parse filename")?;
    let path = path.join(song_path.file_name().context("Cannot create file")?);
    let path_str = path.as_path().to_str().context("Non valid path")?;
    let rec = client.download(
        song.query.filename.clone(),
        song.query.username.clone(),
        song.query.size,
        path_str.to_string(),
    )?;
    let span = tracing::info_span!("download_thread_inner");
    let download_handle: JoinHandle<anyhow::Result<Track>> =
        tokio::task::spawn_blocking(move || {
            let track = span.in_scope(|| {
                loop {
                    let status = rec.recv_timeout(Duration::from_secs(60));
                    match status {
                        Ok(DownloadStatus::Queued) => continue,
                        Ok(DownloadStatus::InProgress {
                            bytes_downloaded,
                            total_bytes,
                            speed_bytes_per_sec,
                        }) if bytes_downloaded % 4 == 0 => {
                            tracing::info!(
                                "Downloaded {} of {} at {} bytes/s for {} ",
                                bytes_downloaded,
                                total_bytes,
                                speed_bytes_per_sec,
                                song.query.filename.clone()
                            );
                            continue;
                        }
                        Ok(DownloadStatus::Completed) => {
                            return Track::File(DownloadedFile {
                                filename: song.query.filename,
                            });
                        }
                        Ok(
                            DownloadStatus::Failed
                            | DownloadStatus::TimedOut
                            | DownloadStatus::InProgress { .. },
                        )
                        | Err(_) => {
                            return Track::Retry(RetryRequest {
                                request: song.clone(),
                                retry_attempts: 0,
                                failed_download_result: song.query,
                            });
                        }
                    }
                }
                // tracing::info!("Reached ending of blocking thread")
            });
            Ok(track)
        });
    let result = download_handle
        .await
        .context("Download thread exiting")?
        .context("Inner")?;
    Ok(result)
}
