use std::{path::PathBuf, sync::Arc};

use soulseek_rs::{Client, DownloadStatus, SearchResult};
use tokio::sync::mpsc;

use crate::internals::search::threaded_search::Status;

// pub enum Status {
//     Done,
//     Downloading,
//     Error,
// }
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
impl Into<Vec<DownloadableFile>> for DownloadableFiles {
    fn into(self) -> Vec<DownloadableFile> {
        self.0
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
    pub async fn run(&mut self) {
        while let Some(song) = self.download_queue.recv().await {
            self.download_track(song).await.unwrap();
        }
        self.status_tx
            .send(Status::Done("song".to_string()))
            .await
            .unwrap();
    }
    async fn download_track(&self, song: DownloadableFile) -> anyhow::Result<()> {
        let path = format!("{:?}", self.root_location);
        println!("Downloading: {:#?}\npath:{}", song, path);
        self.status_tx
            .send(Status::Downloading(path.clone()))
            .await
            .unwrap();
        if let Ok(rec) = self
            .client
            .download(song.filename.clone(), song.username, song.size, path)
        {
            while let Ok(status) = rec.recv() {
                if let DownloadStatus::Completed = status {
                    println!("completado {}", song.filename.clone());
                }
                if let DownloadStatus::Failed | DownloadStatus::TimedOut = status {
                    println!("fallado {}", song.filename.clone());
                }
            }
        }
        Ok(())
    }
}
