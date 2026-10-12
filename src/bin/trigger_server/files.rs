//! Download finished files to the browser: one file at a time, or a whole playlist folder as a
//! zip streamed on the fly (audio is already compressed, so entries are stored, not deflated).

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use actix_files::NamedFile;
use actix_web::http::header::{ContentDisposition, DispositionParam, DispositionType};
use actix_web::{HttpRequest, HttpResponse, get, web};
use bytes::Bytes;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream_shim::ReceiverStream;
use zip::write::SimpleFileOptions;

use crate::errors::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct FileQuery {
    /// Playlist folder, as listed by `/downloads`; absent for files in the download root.
    pub folder: Option<String>,
    pub name: String,
}

#[derive(Deserialize)]
pub struct FolderQuery {
    pub folder: String,
}

/// A single path segment from the client: no separators, no `..`, no hidden names.
fn safe_segment(value: &str) -> ApiResult<&str> {
    let mut components = Path::new(value).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) if !value.starts_with('.') => Ok(value),
        _ => Err(ApiError::BadRequest("invalid file or folder name".into())),
    }
}

fn folder_path(root: &Path, folder: Option<&str>) -> ApiResult<PathBuf> {
    match folder.filter(|f| !f.is_empty()) {
        Some(folder) => Ok(root.join(safe_segment(folder)?)),
        None => Ok(root.to_path_buf()),
    }
}

fn attachment(filename: String) -> ContentDisposition {
    ContentDisposition {
        disposition: DispositionType::Attachment,
        parameters: vec![DispositionParam::Filename(filename)],
    }
}

#[get("/downloads/file")]
pub async fn download_file(
    state: web::Data<AppState>,
    query: web::Query<FileQuery>,
    request: HttpRequest,
) -> ApiResult<HttpResponse> {
    let dir = folder_path(&state.config.download_path, query.folder.as_deref())?;
    let name = safe_segment(&query.name)?;
    let file = NamedFile::open_async(dir.join(name))
        .await
        .map_err(|_| ApiError::NotFound("file not found"))?
        .set_content_disposition(attachment(name.to_string()));
    Ok(file.into_response(&request))
}

/// `std::io::Write` adapter that forwards zip output to the HTTP body in ~256 KiB chunks.
struct ChannelWriter {
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    buf: Vec<u8>,
}

const CHUNK: usize = 256 * 1024;

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.flush()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::take(&mut self.buf));
        self.tx
            .blocking_send(Ok(chunk))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client went away"))
    }
}

fn write_zip(dir: &Path, writer: ChannelWriter) -> std::io::Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'))
                && path.metadata().is_ok_and(|m| m.len() > 0)
        })
        .collect();
    entries.sort();

    let mut zip = zip::ZipWriter::new_stream(writer);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .large_file(true);
    for path in entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        zip.start_file(name, options)?;
        std::io::copy(&mut std::fs::File::open(&path)?, &mut zip)?;
    }
    let mut writer = zip.finish()?.into_inner();
    writer.flush()
}

#[get("/downloads/zip")]
pub async fn download_folder_zip(
    state: web::Data<AppState>,
    query: web::Query<FolderQuery>,
) -> ApiResult<HttpResponse> {
    let folder = safe_segment(&query.folder)?.to_string();
    let dir = folder_path(&state.config.download_path, Some(&folder))?;
    if !dir.is_dir() {
        return Err(ApiError::NotFound("folder not found"));
    }

    let (tx, rx) = mpsc::channel(8);
    let error_tx = tx.clone();
    tokio::task::spawn_blocking(move || {
        let writer = ChannelWriter { tx, buf: Vec::with_capacity(CHUNK) };
        if let Err(err) = write_zip(&dir, writer) {
            tracing::warn!(?err, folder = %dir.display(), "Zip download aborted");
            let _ = error_tx.blocking_send(Err(err));
        }
    });

    Ok(HttpResponse::Ok()
        .content_type("application/zip")
        .insert_header(attachment(format!("{folder}.zip")))
        .streaming(ReceiverStream(rx)))
}

/// Minimal `Stream` over an mpsc receiver (avoids pulling in tokio-stream for one adapter).
mod tokio_stream_shim {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use futures_util::Stream;
    use tokio::sync::mpsc;

    pub struct ReceiverStream<T>(pub mpsc::Receiver<T>);

    impl<T> Stream for ReceiverStream<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
            self.0.poll_recv(cx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::safe_segment;

    #[test]
    fn rejects_traversal_and_hidden_names() {
        assert!(safe_segment("song.flac").is_ok());
        assert!(safe_segment("tech (extended)").is_ok());
        for bad in ["..", "../etc", "a/b", "/etc/passwd", ".env", ""] {
            assert!(safe_segment(bad).is_err(), "{bad} should be rejected");
        }
    }
}
