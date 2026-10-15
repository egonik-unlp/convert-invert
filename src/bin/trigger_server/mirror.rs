//! Mirroring: a collection's folder should hold every song of it that is already downloaded, even
//! when the file was filed under another collection (someone else's playlist, an album, …).
//! Missing ones are hard-linked in (no extra space; the .zip, the file browser and Soulseek all
//! see a plain file), with a relative symlink as the fallback when a hard link isn't possible.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use diesel::prelude::*;
use diesel::sql_types::{Array, Integer, Nullable, Text};

use crate::errors::ApiResult;
use crate::state::AppState;

/// How long a walk of the download tree is reused to match old rows that have no `local_path`.
const NAME_INDEX_TTL: Duration = Duration::from_secs(300);

/// File name -> every file in the download tree with that name.
type NameIndex = HashMap<String, Vec<PathBuf>>;

static NAME_INDEX: Mutex<Option<(Instant, NameIndex)>> = Mutex::new(None);

#[derive(QueryableByName)]
struct OwnedFileRow {
    #[diesel(sql_type = Integer)]
    id: i32,
    #[diesel(sql_type = Text)]
    filename: String,
    #[diesel(sql_type = Nullable<Text>)]
    local_path: Option<String>,
}

/// Link every already-downloaded song among `spotify_ids` into `root/folder`. Returns how many
/// links were created. Never fails the caller's request over a single file: problems are logged.
pub fn mirror_into(state: &AppState, folder: &str, spotify_ids: &[String]) -> ApiResult<usize> {
    if spotify_ids.is_empty() {
        return Ok(0);
    }
    let root = state.config.download_path.clone();
    let target_dir = root.join(folder);
    let mut connection = state.db_pool.get()?;
    let rows = owned_files(&mut connection, spotify_ids)?;

    let mut linked = 0;
    for row in rows {
        let Some(source) = locate(&root, row.id, &row.filename, row.local_path.as_deref(), &mut connection) else {
            continue;
        };
        match link_into(&root, &target_dir, &source) {
            Ok(true) => linked += 1,
            Ok(false) => {}
            Err(err) => tracing::warn!(?err, source = %source.display(), folder, "Could not mirror file"),
        }
    }
    if linked > 0 {
        tracing::info!(folder, linked, "Mirrored already-downloaded songs into folder");
    }
    Ok(linked)
}

/// The file on disk for one downloaded song, if it has one we can find.
pub fn file_for_track(state: &AppState, spotify_id: &str) -> ApiResult<Option<PathBuf>> {
    let mut connection = state.db_pool.get()?;
    let rows = owned_files(&mut connection, &[spotify_id.to_string()])?;
    Ok(rows
        .first()
        .and_then(|row| locate(&state.config.download_path, row.id, &row.filename, row.local_path.as_deref(), &mut connection)))
}

/// The downloaded_file row of each owned song, preferring rows whose location is recorded.
fn owned_files(connection: &mut PgConnection, spotify_ids: &[String]) -> QueryResult<Vec<OwnedFileRow>> {
    diesel::sql_query(
        r#"
        SELECT DISTINCT ON (si.track_id) df.id, df.filename, df.local_path
        FROM search_items si
        JOIN downloaded_file df ON df.track = si.id OR df.filename IN (
            SELECT dlf.filename FROM downloadable_files dlf
            JOIN judge_submissions js ON js.query = dlf.id
            WHERE js.track = si.id
        )
        WHERE si.track_id = ANY($1)
        ORDER BY si.track_id, (df.local_path IS NULL), df.id DESC
        "#,
    )
    .bind::<Array<Text>, _>(spotify_ids)
    .load::<OwnedFileRow>(connection)
}

/// The file on disk for a downloaded row: its recorded path, or for rows from before paths were
/// recorded, the one file in the tree with the remote file's name (written back once found).
pub fn locate(
    root: &Path,
    downloaded_file_id: i32,
    remote_filename: &str,
    local_path: Option<&str>,
    connection: &mut PgConnection,
) -> Option<PathBuf> {
    if let Some(path) = local_path.map(PathBuf::from)
        && path.is_file()
        && path.starts_with(root)
    {
        return Some(path);
    }
    let name = remote_basename(remote_filename)?;
    let found = unique_by_name(root, name)?;
    let _ = diesel::sql_query("UPDATE downloaded_file SET local_path = $1 WHERE id = $2")
        .bind::<Text, _>(found.to_string_lossy())
        .bind::<Integer, _>(downloaded_file_id)
        .execute(connection);
    Some(found)
}

/// Last component of a Soulseek path (`Artist\Album\01 Song.flac` -> `01 Song.flac`).
pub fn remote_basename(filename: &str) -> Option<&str> {
    filename.rsplit(['\\', '/']).next().filter(|name| !name.is_empty())
}

fn unique_by_name(root: &Path, name: &str) -> Option<PathBuf> {
    let mut guard = NAME_INDEX.lock().ok()?;
    let fresh = guard.as_ref().is_some_and(|(built, _)| built.elapsed() < NAME_INDEX_TTL);
    if !fresh {
        *guard = Some((Instant::now(), index_names(root)));
    }
    match guard.as_ref()?.1.get(name).map(Vec::as_slice) {
        Some([only]) => Some(only.clone()),
        _ => None,
    }
}

fn index_names(root: &Path) -> NameIndex {
    let mut index = NameIndex::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                // Mirrored links share their name, so a name in two folders is ambiguous and
                // won't match; rows matched once keep their local_path and never need this again.
                index.entry(entry.file_name().to_string_lossy().into_owned()).or_default().push(path);
            }
        }
    }
    index
}

/// Put `source` into `target_dir` under its own name. `Ok(false)` when it is already there (as
/// the same file, or a file of that name that we leave alone).
pub fn link_into(root: &Path, target_dir: &Path, source: &Path) -> std::io::Result<bool> {
    let Some(name) = source.file_name() else {
        return Ok(false);
    };
    let dest = target_dir.join(name);
    if source.parent() == Some(target_dir) || dest.symlink_metadata().is_ok() {
        return Ok(false);
    }
    std::fs::create_dir_all(target_dir)?;
    match std::fs::hard_link(source, &dest) {
        Ok(()) => Ok(true),
        Err(hard_err) => {
            let relative = source
                .strip_prefix(root)
                .map(|inside| Path::new("..").join(inside))
                .map_err(|_| hard_err)?;
            std::os::unix::fs::symlink(relative, &dest)?;
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_basename_handles_soulseek_paths() {
        assert_eq!(remote_basename(r"Music\Kid A\01 Everything.flac"), Some("01 Everything.flac"));
        assert_eq!(remote_basename("plain.mp3"), Some("plain.mp3"));
        assert_eq!(remote_basename(r"ends\with\"), None);
    }

    #[test]
    fn link_into_hard_links_once_and_skips_the_home_folder() {
        let root = std::env::temp_dir().join(format!("ci-mirror-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("Your Mix");
        std::fs::create_dir_all(&home).unwrap();
        let source = home.join("song.flac");
        std::fs::write(&source, b"audio").unwrap();

        let other = root.join("Their Mix");
        assert!(link_into(&root, &other, &source).unwrap());
        assert_eq!(std::fs::read(other.join("song.flac")).unwrap(), b"audio");
        assert!(!link_into(&root, &other, &source).unwrap(), "second time is a no-op");
        assert!(!link_into(&root, &home, &source).unwrap(), "never links a file onto itself");

        // Removing the original keeps the mirrored copy (a hard link, not a pointer).
        std::fs::remove_file(&source).unwrap();
        assert_eq!(std::fs::read(other.join("song.flac")).unwrap(), b"audio");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
