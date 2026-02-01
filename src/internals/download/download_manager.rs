use crate::internals::{
    context::context_manager::{
        DownloadedFile, RejectReason, RejectedTrack, RetryRequest, Track, send,
    },
    search::search_manager::JudgeSubmission,
};
use anyhow::Context;
use soulseek_rs::{Client, DownloadStatus};
use std::{path::PathBuf, str::FromStr, sync::Arc, time::Duration};
use tokio::{
    sync::{Semaphore, mpsc::Sender},
    task::JoinHandle,
};

fn is_audio_file(filename: String) -> bool {
    let lc = filename.to_lowercase();
    lc.ends_with(".mp3") || lc.ends_with(".flac") || lc.ends_with(".aiff")
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
    pub async fn run(
        &self,
        track: JudgeSubmission,
        semaphore: Arc<Semaphore>,
        sender: Arc<Sender<Track>>,
    ) -> anyhow::Result<()> {
        let client = Arc::clone(&self.client);
        let download_location = self.root_location.clone();
        if is_audio_file(track.query.filename.clone()) {
            let _permit = semaphore.acquire().await.context("acquiring semaphore")?;
            tracing::info!(track.query.filename, "send to download");
            let track = download_track(track, download_location.clone(), client)
                .await
                .context("Downloading track")?;
            send(track, &sender).await.context("Sending to finish")?;
        } else {
            let reject = RejectedTrack::new(
                track.clone(),
                RejectReason::NotMusic(track.query.filename.clone()),
            );
            send(Track::Reject(reject), &sender)
                .await
                .context("Rejection sending to chan")?;
            tracing::info!(
                track.query.filename,
                "Rejected non song & already downloaded file",
            );
        }
        Ok(())
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
                        Ok(DownloadStatus::Failed | DownloadStatus::TimedOut) | Err(_) => {
                            tracing::error!(?song, "Error descargando, se salio del loop");
                            return Track::Retry(RetryRequest {
                                request: song.clone(),
                                retry_attempts: 0,
                                failed_download_result: song.query,
                            });
                        }
                        _ => continue,
                    }
                }
            });
            Ok(track)
        });
    let result = download_handle
        .await
        .context("Download thread exiting")?
        .context("Inner")?;
    Ok(result)
}
