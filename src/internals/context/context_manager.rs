use tokio::sync::mpsc;

use crate::internals::search::search_manager::{
    DownloadableFile, OwnSearchResult, SearchItem, Status,
};

#[allow(dead_code)]
#[derive(Debug)]
pub struct ContextManager {
    status: mpsc::Receiver<Status>,
}

#[derive(Debug)]
pub struct DownloadedFile;

#[derive(Debug)]
pub enum Track {
    Query(SearchItem),
    Result(SearchItem, OwnSearchResult),
    Downloadable(DownloadableFile),
    File(DownloadedFile),
}
pub trait Manager {
    fn run(self) -> anyhow::Result<()>;
}
