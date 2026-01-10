use tokio::sync::mpsc;

use crate::internals::{
    download::download_manager::DownloadableFile,
    search::search_manager::{OwnSearchResult, SearchItem, Status},
};

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
