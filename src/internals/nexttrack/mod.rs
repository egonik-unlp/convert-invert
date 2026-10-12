//! Playlist extension with the Infinite Playlist next-track model.
//!
//! The model is the anti-eager next-track GRU (`seq-nexttrack`, `eager_beta 0.1`) from the
//! spotify-next-track lab, exported to ONNX together with its retrieval assets:
//!
//! - `gru.onnx`: prefix of PCA-192 track latents `(1, T, 192)` -> L2-normalized next latent `(1, 192)`.
//! - `latents.i16`: one quantized PCA-192 latent per catalog row (`value * manifest.scale`).
//! - `catalog.json`: `{uri, name, artist, genre, il}` per row; `il = 1` marks library tracks,
//!   the only ones a journey may pick.
//! - `manifest.json`: `{n, dim, scale}`.
//!
//! Generation mirrors the browser app (`clients/infinite-playlist/public/app.js`): seed tracks
//! that exist in the catalog are reduced to their dominant mood core, the GRU predicts the next
//! latent, the nearest unused library track wins (cosine, plus a pull towards the core centroid,
//! minus a penalty for artists already played), and the pick is appended to the prefix. Greedy
//! and deterministic, so a preview and the download it launches produce the same tracks.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, bail};
use serde::Deserialize;
use tract_onnx::prelude::*;

/// Env var naming the directory holding the model assets.
pub const MODEL_DIR_ENV: &str = "NEXTTRACK_MODEL_DIR";
const DEFAULT_MODEL_DIR: &str = "assets/infinite-playlist";

/// How hard the walk holds the seed mood (app.js `ANCHOR_L`).
const ANCHOR_WEIGHT: f32 = 0.4;
/// Score penalty for an artist already in the journey (app.js penalty slider default).
pub const DEFAULT_ARTIST_PENALTY: f32 = 0.5;
/// Upper bound on generated tracks per request.
pub const MAX_EXTEND_COUNT: usize = 500;

type Plan = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogEntry {
    pub uri: String,
    pub name: String,
    pub artist: String,
    #[serde(default)]
    pub genre: String,
    #[serde(default = "default_il")]
    pub il: u8,
}

fn default_il() -> u8 {
    1
}

impl CatalogEntry {
    /// Bare Spotify track id (`spotify:track:<id>` -> `<id>`).
    pub fn track_id(&self) -> &str {
        self.uri.rsplit(':').next().unwrap_or(&self.uri)
    }
}

#[derive(Deserialize)]
struct Manifest {
    n: usize,
    dim: usize,
    scale: f32,
}

pub struct NextTrackModel {
    plan: Plan,
    dim: usize,
    latents: Vec<f32>,
    norms: Vec<f32>,
    catalog: Vec<CatalogEntry>,
    by_track_id: HashMap<String, usize>,
    by_name_artist: HashMap<(String, String), usize>,
}

/// A seed track as fetched from Spotify; matched against the catalog by id, then by name+artist.
#[derive(Debug, Clone)]
pub struct SeedTrack {
    pub track_id: String,
    pub name: String,
    pub artist: String,
}

#[derive(Debug, Clone)]
pub struct Extension {
    /// Seeds found in the model's catalog.
    pub seeds_matched: usize,
    /// Size of the dominant-mood core the journey starts from.
    pub core_size: usize,
    pub tracks: Vec<CatalogEntry>,
}

static MODEL: OnceLock<Result<Arc<NextTrackModel>, String>> = OnceLock::new();

/// The process-wide model, loaded on first use from `$NEXTTRACK_MODEL_DIR`.
pub fn shared_model() -> anyhow::Result<Arc<NextTrackModel>> {
    MODEL
        .get_or_init(|| {
            let dir = std::env::var(MODEL_DIR_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(DEFAULT_MODEL_DIR));
            NextTrackModel::load(&dir)
                .map(Arc::new)
                .map_err(|err| format!("{err:#}"))
        })
        .clone()
        .map_err(|err| anyhow::anyhow!("Next-track model unavailable: {err}"))
}

fn normalize_key(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

impl NextTrackModel {
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let manifest: Manifest = serde_json::from_slice(
            &std::fs::read(dir.join("manifest.json"))
                .with_context(|| format!("Read {}/manifest.json", dir.display()))?,
        )
        .context("Parse manifest.json")?;
        let catalog: Vec<CatalogEntry> = serde_json::from_slice(
            &std::fs::read(dir.join("catalog.json")).context("Read catalog.json")?,
        )
        .context("Parse catalog.json")?;
        let raw = std::fs::read(dir.join("latents.i16")).context("Read latents.i16")?;

        let (n, dim) = (manifest.n, manifest.dim);
        if catalog.len() != n || raw.len() != n * dim * 2 {
            bail!(
                "Model assets disagree: manifest n={n} dim={dim}, catalog {} rows, latents {} bytes",
                catalog.len(),
                raw.len()
            );
        }
        let latents: Vec<f32> = raw
            .chunks_exact(2)
            .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) * manifest.scale)
            .collect();
        let norms = latents
            .chunks_exact(dim)
            .map(|row| {
                let norm = row.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm == 0.0 { 1.0 } else { norm }
            })
            .collect();

        let plan = tract_onnx::onnx()
            .model_for_path(dir.join("gru.onnx"))
            .context("Load gru.onnx")?
            .into_optimized()
            .context("Optimize gru.onnx")?
            .into_runnable()
            .context("Plan gru.onnx")?;

        let mut by_track_id = HashMap::with_capacity(n);
        let mut by_name_artist = HashMap::with_capacity(n);
        for (row, entry) in catalog.iter().enumerate() {
            by_track_id.insert(entry.track_id().to_string(), row);
            by_name_artist
                .entry((normalize_key(&entry.name), normalize_key(&entry.artist)))
                .or_insert(row);
        }

        tracing::info!(tracks = n, dim, dir = %dir.display(), "Loaded next-track model");
        Ok(Self {
            plan,
            dim,
            latents,
            norms,
            catalog,
            by_track_id,
            by_name_artist,
        })
    }

    fn latent(&self, row: usize) -> &[f32] {
        &self.latents[row * self.dim..(row + 1) * self.dim]
    }

    fn unit(&self, row: usize) -> Vec<f32> {
        self.latent(row).iter().map(|v| v / self.norms[row]).collect()
    }

    fn match_seed(&self, seed: &SeedTrack) -> Option<usize> {
        self.by_track_id.get(&seed.track_id).copied().or_else(|| {
            self.by_name_artist
                .get(&(normalize_key(&seed.name), normalize_key(&seed.artist)))
                .copied()
        })
    }

    fn predict_next(&self, prefix: &[usize]) -> anyhow::Result<Vec<f32>> {
        let mut data = Vec::with_capacity(prefix.len() * self.dim);
        for &row in prefix {
            data.extend_from_slice(self.latent(row));
        }
        let input = tract_ndarray::Array3::from_shape_vec((1, prefix.len(), self.dim), data)?;
        let out = self.plan.run(tvec!(Tensor::from(input).into()))?;
        Ok(out[0].as_slice::<f32>()?.to_vec())
    }

    /// The dominant sub-cluster of a (possibly broad) seed set: the medoid plus every seed at
    /// least as similar to it as average, so outlier sub-genres don't steer the journey.
    fn dominant_core(&self, rows: &[usize]) -> Vec<usize> {
        let n = rows.len();
        if n <= 10 {
            return rows.to_vec();
        }
        let units: Vec<Vec<f32>> = rows.iter().map(|&r| self.unit(r)).collect();
        let sim = |a: usize, b: usize| units[a].iter().zip(&units[b]).map(|(x, y)| x * y).sum::<f32>();
        let medoid = (0..n)
            .map(|i| (i, (0..n).filter(|&k| k != i).map(|k| sim(i, k)).sum::<f32>()))
            .fold((0, f32::NEG_INFINITY), |best, cur| if cur.1 > best.1 { cur } else { best })
            .0;
        let to_medoid: Vec<f32> = (0..n).map(|i| if i == medoid { 1.0 } else { sim(medoid, i) }).collect();
        let mean = to_medoid.iter().sum::<f32>() / n as f32;
        let mut keep: Vec<usize> = (0..n).filter(|&i| to_medoid[i] >= mean).collect();
        let floor = 8.max((0.4 * n as f32).round() as usize);
        if keep.len() < floor {
            let mut ranked: Vec<usize> = (0..n).collect();
            ranked.sort_by(|&a, &b| to_medoid[b].total_cmp(&to_medoid[a]));
            keep = ranked.into_iter().take(floor).collect();
        }
        keep.into_iter().map(|i| rows[i]).collect()
    }

    fn centroid(&self, rows: &[usize]) -> Vec<f32> {
        let mut c = vec![0.0f32; self.dim];
        for &r in rows {
            for (acc, v) in c.iter_mut().zip(self.unit(r)) {
                *acc += v;
            }
        }
        let norm = c.iter().map(|v| v * v).sum::<f32>().sqrt();
        let norm = if norm == 0.0 { 1.0 } else { norm };
        c.iter_mut().for_each(|v| *v /= norm);
        c
    }

    /// Generate `count` library tracks continuing the mood of `seeds`. Seeds that aren't in the
    /// model's catalog are ignored; it is an error if none of them are.
    pub fn extend(&self, seeds: &[SeedTrack], count: usize, artist_penalty: f32) -> anyhow::Result<Extension> {
        let mut seed_rows = Vec::new();
        for seed in seeds {
            if let Some(row) = self.match_seed(seed)
                && !seed_rows.contains(&row)
            {
                seed_rows.push(row);
            }
        }
        if seed_rows.is_empty() {
            bail!(
                "None of the {} seed tracks are in the next-track model's library, so there is nothing to extend from",
                seeds.len()
            );
        }

        let core = self.dominant_core(&seed_rows);
        let anchor = (core.len() >= 5).then(|| self.centroid(&core));
        let mut prefix = core.clone();
        let mut used: HashSet<usize> = seed_rows.iter().copied().collect();
        let mut artists: HashSet<&str> = core.iter().map(|&r| self.catalog[r].artist.as_str()).collect();
        let mut picks = Vec::with_capacity(count);

        for _ in 0..count {
            let pred = self.predict_next(&prefix)?;
            let mut best: Option<(usize, f32)> = None;
            for (row, entry) in self.catalog.iter().enumerate() {
                if entry.il == 0 || used.contains(&row) {
                    continue;
                }
                let latent = self.latent(row);
                let mut score = dot(latent, &pred) / self.norms[row];
                if let Some(anchor) = &anchor {
                    score += ANCHOR_WEIGHT * dot(latent, anchor) / self.norms[row];
                }
                if artists.contains(entry.artist.as_str()) {
                    score -= artist_penalty;
                }
                if best.is_none_or(|(_, b)| score > b) {
                    best = Some((row, score));
                }
            }
            let Some((row, _)) = best else { break };
            picks.push(row);
            prefix.push(row);
            used.insert(row);
            artists.insert(self.catalog[row].artist.as_str());
        }

        Ok(Extension {
            seeds_matched: seed_rows.len(),
            core_size: core.len(),
            tracks: picks.into_iter().map(|r| self.catalog[r].clone()).collect(),
        })
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Needs the model assets; run from the crate root with:
    //   cargo test --lib nexttrack -- --ignored --nocapture
    #[test]
    #[ignore]
    fn extends_from_a_catalog_seed() {
        let model = NextTrackModel::load(Path::new(DEFAULT_MODEL_DIR)).expect("load model");
        let seed = model.catalog.iter().find(|c| c.il == 1).unwrap().clone();
        let seeds = [SeedTrack {
            track_id: seed.track_id().to_string(),
            name: seed.name.clone(),
            artist: seed.artist.clone(),
        }];
        let ext = model.extend(&seeds, 12, DEFAULT_ARTIST_PENALTY).expect("extend");
        assert_eq!(ext.seeds_matched, 1);
        assert_eq!(ext.tracks.len(), 12);
        assert!(ext.tracks.iter().all(|t| t.il == 1 && t.uri != seed.uri));
        println!("seed: {} — {}", seed.name, seed.artist);
        for t in &ext.tracks {
            println!("  {} — {} [{}]", t.name, t.artist, t.genre);
        }
    }
}
