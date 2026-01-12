use crate::internals::search::search_manager::{DownloadableFile, Status};
use anyhow::Context;
use soulseek_rs::{Client, DownloadStatus, SearchResult};
use std::{path::PathBuf, str::FromStr, sync::Arc, time::Duration};
use tokio::{
    sync::{Semaphore, mpsc},
    task::{JoinHandle, JoinSet},
    time::sleep,
};
use tracing::{Instrument, Level, instrument, span};

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
    status_tx: mpsc::Sender<Status>,
    download_queue: mpsc::Receiver<DownloadableFile>,
}

impl DownloadManager {
    pub fn new(
        client: Arc<Client>,
        root_location: PathBuf,
        status_tx: mpsc::Sender<Status>,
        download_queue: mpsc::Receiver<DownloadableFile>,
    ) -> Self {
        DownloadManager {
            client,
            root_location,
            status_tx,
            download_queue,
        }
    }
    #[instrument(name = "DownloadManager::run", skip(self))]
    pub async fn run(&mut self) -> anyhow::Result<()> {
        let max_parallel_downloads = 3usize;
        // self.download_queue
        let sem = Arc::new(Semaphore::new(max_parallel_downloads));
        let mut set: JoinSet<anyhow::Result<()>> = JoinSet::new();
        while let Some(song) = self.download_queue.recv().await {
            if is_audio_file(song.filename.clone()) {
                let permit = sem.acquire().await.context("Permit request denied")?;
                set.spawn(async {
                    download_track(self, song)
                        .await
                        .context("Downloading track")?;
                    let _permit = permit;
                    Ok(())
                });
                tracing::info!("After download track in run");
            } else {
                tracing::info!("Rejected non song file = {}", song.filename)
            }
        }
        Ok(())
    }
}

#[tracing::instrument(name = "DownloadManager::download_track", skip(download_manager, song), fields(track=song.filename, username = song.username))]
async fn download_track(
    download_manager: &DownloadManager,
    song: DownloadableFile,
) -> anyhow::Result<()> {
    let song_path = PathBuf::from_str(&song.filename).context("Can't parse filename")?;
    //TODO: Solve unwrap here
    let path = download_manager
        .root_location
        .join(song_path.file_name().unwrap());
    let path_str = path.as_path().to_str().context("Non valid path")?;
    tracing::info!("\n\nfullpath: {:#?}\npath:{}", song, path_str);
    if let Ok(rec) = download_manager.client.download(
        song.filename.clone(),
        song.username,
        song.size,
        path_str.to_string(),
    ) {
        let span = tracing::info_span!("download_thread");
        let download_handle = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                while let Ok(status) = rec.recv() {
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
        tracing::info!("Pre await download handle");
        download_handle.await.context("Download thread down")?;
        tracing::info!("Post await download handle");
    }
    Ok(())
}
