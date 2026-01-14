#![allow(unused, dead_code)]
use crate::internals::{parsing::deserialize, search::search_manager::SearchItem};
use anyhow::Context;
use std::time::Duration;
use tokio::{sync::mpsc::Sender, time::sleep};

pub struct QueryManager {
    playlist_url: String,
    data_tx: Sender<SearchItem>,
}

impl QueryManager {
    pub fn new(playlist_url: impl Into<String>, data_tx: Sender<SearchItem>) -> Self {
        let playlist_url = playlist_url.into();
        QueryManager {
            playlist_url,
            data_tx,
        }
    }
    async fn get_playlist_from_spotify(&self) -> anyhow::Result<()> {
        todo!()
    }
    pub async fn run(self) -> anyhow::Result<()> {
        let data_string = include_str!("../parsing/sample.json");
        let data: deserialize::Playlist = serde_json::from_str(data_string).unwrap();
        let queries: Vec<SearchItem> = data.into();
        let start_time = chrono::Local::now();
        for (n, song) in queries.into_iter().skip(4).enumerate() {
            self.data_tx
                .send(song.clone())
                .await
                .context("Sending query song")?;
            let elapsed = (chrono::Local::now() - start_time).as_seconds_f32();
            tracing::warn!("File Sent: {:?}\nHTP = {}", song, elapsed);
            if n % 10 == 0 && n > 1 {
                sleep(Duration::from_secs(200)).await;
            }
        }
        Ok(())
    }
}
