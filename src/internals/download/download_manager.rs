use crate::internals::search::search_manager::Status;
use anyhow::Context;
use soulseek_rs::{Client, DownloadStatus, SearchResult};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::mpsc, time::sleep};
use tracing::instrument;

#[derive(Debug)]
pub struct DownloadableFile {
    pub filename: String,
    pub username: String,
    pub size: u64,
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
        while let Some(song) = self.download_queue.recv().await {
            self.download_track(song)
                .await
                .context("Downloading track")?;
        }
        self.status_tx
            .send(Status::Done("song".to_string()))
            .await
            .context("Sending status")?;
        Ok(())
    }

    #[tracing::instrument(name = "DownloadManager::download_track", skip(self))]
    async fn download_track(&self, song: DownloadableFile) -> anyhow::Result<()> {
        // let path = format!("{:?}", self.root_location);
        let path = self.root_location.join(song.filename.clone());
        let path_str = path.as_path().to_str().context("Non valid path")?;
        tracing::info!("Downloading: {:#?}\npath:{}", song, path_str);
        self.status_tx
            .send(Status::Downloading(path_str.to_string()))
            .await
            .context("sending status")?;
        if let Ok(rec) = self.client.download(
            song.filename.clone(),
            song.username,
            song.size,
            path_str.to_string(),
        ) {
            while let Ok(status) = rec.recv() {
                if let DownloadStatus::Completed = status {
                    tracing::info!("completado {}", song.filename.clone());
                }
                if let DownloadStatus::Failed | DownloadStatus::TimedOut = status {
                    tracing::info!("fallado {}", song.filename.clone());
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

                sleep(Duration::from_secs(10)).await
            }
        }
        Ok(())
    }
}
