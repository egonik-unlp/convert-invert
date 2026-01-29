use tokio::sync::mpsc::{Receiver, Sender};

use crate::internals::search::search_manager::{
    DownloadableFile, OwnSearchResult, SearchItem, Status,
};

#[allow(dead_code)]
#[derive(Debug)]
pub struct ContextManager {
    status: Receiver<Status>,
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

//TODO: Esto es un mock, hay que completarlo
#[allow(dead_code, unused_variables)]
pub async fn run(tx: Sender<Track>, mut rx: Receiver<Track>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            Track::Query(search_item) => {
                let resultado: Track = busqueda(search_item);
                tx.send(resultado).await.unwrap();
            }
            Track::Result(search_item, own_search_result) => todo!(),
            Track::Downloadable(downloadable_file) => todo!(),
            Track::File(downloaded_file) => todo!(),
        }
    }
}

#[allow(dead_code, unused_variables)]
fn busqueda(search_item: SearchItem) -> Track {
    todo!()
}
