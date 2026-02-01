//TODO: Remove lint allows
#![allow(unused, dead_code)]
use crate::internals::{
    context::context_manager::Track, parsing::deserialize, search::search_manager::SearchItem,
};
use anyhow::Context;
use rand::Rng;
use std::time::Duration;
use tokio::{sync::mpsc::Sender, time::sleep};
pub struct QueryManager {
    playlist_url: String,
}

impl QueryManager {
    pub fn new(playlist_url: impl Into<String>) -> Self {
        let playlist_url = playlist_url.into();
        QueryManager { playlist_url }
    }
    async fn get_playlist_from_spotify(&self) -> anyhow::Result<()> {
        todo!()
    }
    pub async fn run(&self) -> anyhow::Result<Vec<Track>> {
        let data_string = include_str!("../parsing/sample.json");
        let data: deserialize::Playlist =
            serde_json::from_str(data_string).context("Deserializing")?;
        let queries: Vec<SearchItem> = data.into();
        let vals = queries.into_iter().map(Track::Query).collect();
        Ok(vals)
    }
}
