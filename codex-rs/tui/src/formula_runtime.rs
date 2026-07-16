use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

use lru::LruCache;
use sha2::Digest;
use sha2::Sha256;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::formula_parser::FormulaKind;
use crate::formula_parser::scan_formulas;
use crate::formula_render::FormulaLayoutRaster;
use crate::formula_render::FormulaLayoutTarget;
use crate::formula_render::FormulaRenderer;

const MAX_FORMULAS_PER_MESSAGE: usize = 128;
const FORMULA_JOB_TIMEOUT: Duration = Duration::from_secs(5);
const MEMORY_CACHE_BYTES: usize = 64 * 1024 * 1024;
const CACHE_MAGIC: &[u8; 4] = b"CFM5";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct FormulaRenderKey {
    pub(crate) width: u16,
    pub(crate) cell_pixel_width: u16,
    pub(crate) cell_pixel_height: u16,
    pub(crate) foreground_rgb: [u8; 3],
}

#[derive(Clone, Debug)]
pub(crate) struct FormulaSource {
    pub(crate) source_range: Range<usize>,
    pub(crate) body: String,
    pub(crate) kind: FormulaKind,
}

#[derive(Clone, Debug)]
pub(crate) struct FormulaAsset {
    pub(crate) source: FormulaSource,
    pub(crate) layout: FormulaLayoutRaster,
}

#[derive(Debug)]
enum RenderVariant {
    Pending,
    Ready(Vec<Result<FormulaAsset, String>>),
}

#[derive(Debug)]
pub(crate) struct FormulaMessageState {
    formulas: Vec<FormulaSource>,
    variants: Mutex<HashMap<FormulaRenderKey, RenderVariant>>,
    active_key: Mutex<Option<FormulaRenderKey>>,
    render_generation: AtomicU64,
    reported_error_widths: Mutex<HashSet<u16>>,
}

impl FormulaMessageState {
    pub(crate) fn new(markdown_source: &str) -> Arc<Self> {
        let formulas = scan_formulas(markdown_source)
            .into_iter()
            .take(MAX_FORMULAS_PER_MESSAGE)
            .map(|formula| FormulaSource {
                source_range: formula.source_range,
                body: formula.body.to_string(),
                kind: formula.kind,
            })
            .collect();
        Arc::new(Self {
            formulas,
            variants: Mutex::new(HashMap::new()),
            active_key: Mutex::new(None),
            render_generation: AtomicU64::new(0),
            reported_error_widths: Mutex::new(HashSet::new()),
        })
    }

    pub(crate) fn has_formulas(&self) -> bool {
        !self.formulas.is_empty()
    }

    pub(crate) fn prepare(
        self: &Arc<Self>,
        key: FormulaRenderKey,
        cell_id: u64,
        app_event_tx: AppEventSender,
    ) -> bool {
        if self.formulas.is_empty() {
            return false;
        }
        *self
            .active_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(key);
        let mut variants = self
            .variants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if variants.contains_key(&key) {
            return true;
        }
        variants.clear();
        variants.insert(key, RenderVariant::Pending);
        drop(variants);
        let generation = self.render_generation.fetch_add(1, Ordering::Relaxed) + 1;

        formula_worker().send(RenderJob {
            state: self.clone(),
            key,
            generation,
            cell_id,
            app_event_tx,
        });
        true
    }

    pub(crate) fn ready_assets(&self, width: u16) -> Option<Vec<Result<FormulaAsset, String>>> {
        let active_key = self
            .active_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .filter(|key| key.width == width)?;
        let variants = self
            .variants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match variants.get(&active_key)? {
            RenderVariant::Pending => None,
            RenderVariant::Ready(assets) => Some(assets.clone()),
        }
    }

    pub(crate) fn deactivate(&self) {
        self.render_generation.fetch_add(1, Ordering::Relaxed);
        *self
            .active_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.variants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    pub(crate) fn is_ready(&self, key: FormulaRenderKey) -> bool {
        matches!(
            self.variants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&key),
            Some(RenderVariant::Ready(_))
        )
    }

    pub(crate) fn take_errors(&self, width: u16) -> Vec<String> {
        let mut reported = self
            .reported_error_widths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !reported.insert(width) {
            return Vec::new();
        }
        let active_key = self
            .active_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .filter(|key| key.width == width);
        let variants = self
            .variants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match active_key.and_then(|key| variants.get(&key)) {
            Some(RenderVariant::Ready(assets)) => assets
                .iter()
                .filter_map(|asset| asset.as_ref().err().cloned())
                .collect(),
            _ => Vec::new(),
        }
    }
}

struct RenderJob {
    state: Arc<FormulaMessageState>,
    key: FormulaRenderKey,
    generation: u64,
    cell_id: u64,
    app_event_tx: AppEventSender,
}

struct FormulaWorker {
    tx: mpsc::Sender<RenderJob>,
}

impl FormulaWorker {
    fn send(&self, job: RenderJob) {
        if let Err(error) = self.tx.send(job) {
            tracing::warn!(%error, "formula render worker stopped");
        }
    }
}

fn formula_worker() -> &'static FormulaWorker {
    static WORKER: OnceLock<FormulaWorker> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<RenderJob>();
        std::thread::Builder::new()
            .name("codex-formula-render".to_string())
            .spawn(move || run_formula_worker(rx))
            .unwrap_or_else(|error| panic!("failed to start formula render worker: {error}"));
        FormulaWorker { tx }
    })
}

fn run_formula_worker(rx: mpsc::Receiver<RenderJob>) {
    let renderer = FormulaRenderer::new();
    while let Ok(job) = rx.recv() {
        if !render_job_is_current(&job) {
            continue;
        }
        let job_deadline = Instant::now() + FORMULA_JOB_TIMEOUT;
        let assets = match &renderer {
            Ok(renderer) => {
                let mut assets = Vec::with_capacity(job.state.formulas.len());
                for (index, source) in job.state.formulas.iter().enumerate() {
                    if !render_job_is_current(&job) {
                        break;
                    }
                    let now = Instant::now();
                    if now >= job_deadline {
                        assets.extend((index..job.state.formulas.len()).map(|_| {
                            Err("formula message exceeded the 5 second rendering boundary"
                                .to_string())
                        }));
                        break;
                    }
                    let cache_key = formula_cache_key(source, job.key);
                    let layout = if let Some(layout) = memory_cache_get(cache_key) {
                        Ok(layout)
                    } else if let Some(layout) = load_cached_layout(source, job.key) {
                        memory_cache_put(cache_key, layout.clone());
                        Ok(layout)
                    } else {
                        renderer
                            .render_for_layout(
                                &source.body,
                                source.kind,
                                FormulaLayoutTarget {
                                    max_columns: job.key.width.saturating_sub(2).max(1),
                                    cell_pixel_width: job.key.cell_pixel_width,
                                    cell_pixel_height: job.key.cell_pixel_height,
                                    foreground_rgb: job.key.foreground_rgb,
                                    render_timeout: (job_deadline - now)
                                        .min(crate::formula_render::FORMULA_RENDER_TIMEOUT),
                                },
                            )
                            .inspect(|layout| {
                                memory_cache_put(cache_key, layout.clone());
                                write_cached_layout(source, job.key, layout);
                            })
                    };
                    assets.push(
                        layout
                            .map(|layout| FormulaAsset {
                                source: source.clone(),
                                layout,
                            })
                            .map_err(|error| error.to_string()),
                    );
                }
                assets
            }
            Err(error) => job
                .state
                .formulas
                .iter()
                .map(|_| Err(error.to_string()))
                .collect(),
        };
        let active_key = job
            .state
            .active_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *active_key == Some(job.key)
            && job.state.render_generation.load(Ordering::Relaxed) == job.generation
        {
            job.state
                .variants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(job.key, RenderVariant::Ready(assets));
            drop(active_key);
            job.app_event_tx.send(AppEvent::FormulaRenderReady {
                cell_id: job.cell_id,
            });
        }
    }
}

fn render_job_is_current(job: &RenderJob) -> bool {
    job.state.render_generation.load(Ordering::Relaxed) == job.generation
        && *job
            .state
            .active_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            == Some(job.key)
}

struct FormulaMemoryCache {
    entries: LruCache<[u8; 32], FormulaLayoutRaster>,
    bytes: usize,
    max_bytes: usize,
}

impl FormulaMemoryCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            entries: LruCache::unbounded(),
            bytes: 0,
            max_bytes,
        }
    }

    fn get(&mut self, key: &[u8; 32]) -> Option<FormulaLayoutRaster> {
        self.entries.get(key).cloned()
    }

    fn put(&mut self, key: [u8; 32], layout: FormulaLayoutRaster) {
        let bytes = layout.raster.png.len();
        if let Some(previous) = self.entries.pop(&key) {
            self.bytes -= previous.raster.png.len();
        }
        if bytes > self.max_bytes {
            return;
        }
        while self.bytes + bytes > self.max_bytes {
            let Some((_, evicted)) = self.entries.pop_lru() else {
                break;
            };
            self.bytes -= evicted.raster.png.len();
        }
        self.entries.put(key, layout);
        self.bytes += bytes;
    }
}

fn formula_memory_cache() -> &'static Mutex<FormulaMemoryCache> {
    static CACHE: OnceLock<Mutex<FormulaMemoryCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(FormulaMemoryCache::new(MEMORY_CACHE_BYTES)))
}

fn memory_cache_get(key: [u8; 32]) -> Option<FormulaLayoutRaster> {
    formula_memory_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
}

fn memory_cache_put(key: [u8; 32], layout: FormulaLayoutRaster) {
    formula_memory_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .put(key, layout);
}

fn formula_cache_key(source: &FormulaSource, key: FormulaRenderKey) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(
        b"codex-tui-math-v5-rquickjs-0.11-resvg-0.47-transparent-cell-font-centered-stroke-24",
    );
    hasher.update([match source.kind {
        FormulaKind::Inline => 0,
        FormulaKind::Display => 1,
    }]);
    hasher.update(source.body.as_bytes());
    hasher.update(key.width.to_le_bytes());
    hasher.update(key.cell_pixel_width.to_le_bytes());
    hasher.update(key.cell_pixel_height.to_le_bytes());
    hasher.update(key.foreground_rgb);
    hasher.finalize().into()
}

fn formula_cache_path(source: &FormulaSource, key: FormulaRenderKey) -> Option<PathBuf> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digest = String::with_capacity(64);
    for byte in formula_cache_key(source, key) {
        digest.push(HEX[usize::from(byte >> 4)] as char);
        digest.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    let home = codex_utils_home_dir::find_codex_home().ok()?;
    Some(
        home.as_path()
            .join("cache")
            .join("tui-math")
            .join("v5")
            .join(format!("{digest}.bin")),
    )
}

fn load_cached_layout(
    source: &FormulaSource,
    key: FormulaRenderKey,
) -> Option<FormulaLayoutRaster> {
    let bytes = fs::read(formula_cache_path(source, key)?).ok()?;
    if bytes.len() < 25 || &bytes[..4] != CACHE_MAGIC || &bytes[17..25] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let columns = u16::from_le_bytes(bytes[4..6].try_into().ok()?);
    let rows = u16::from_le_bytes(bytes[6..8].try_into().ok()?);
    let is_block = bytes[8] != 0;
    let pixel_width = u32::from_le_bytes(bytes[9..13].try_into().ok()?);
    let pixel_height = u32::from_le_bytes(bytes[13..17].try_into().ok()?);
    let expected_width = u32::from(columns) * u32::from(key.cell_pixel_width) * 2;
    let expected_height = u32::from(rows) * u32::from(key.cell_pixel_height) * 2;
    if pixel_width != expected_width || pixel_height != expected_height {
        return None;
    }
    Some(FormulaLayoutRaster {
        raster: crate::formula_render::FormulaRaster {
            png: Arc::from(bytes[17..].to_vec()),
            pixel_width,
            pixel_height,
        },
        columns,
        rows,
        is_block,
    })
}

fn write_cached_layout(
    source: &FormulaSource,
    key: FormulaRenderKey,
    layout: &FormulaLayoutRaster,
) {
    let Some(path) = formula_cache_path(source, key) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if let Err(error) = fs::create_dir_all(parent) {
        tracing::debug!(%error, "failed to create formula cache directory");
        return;
    }
    let mut bytes = Vec::with_capacity(17 + layout.raster.png.len());
    bytes.extend_from_slice(CACHE_MAGIC);
    bytes.extend_from_slice(&layout.columns.to_le_bytes());
    bytes.extend_from_slice(&layout.rows.to_le_bytes());
    bytes.push(u8::from(layout.is_block));
    bytes.extend_from_slice(&layout.raster.pixel_width.to_le_bytes());
    bytes.extend_from_slice(&layout.raster.pixel_height.to_le_bytes());
    bytes.extend_from_slice(&layout.raster.png);
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if let Err(error) = fs::write(&temporary, bytes).and_then(|()| fs::rename(&temporary, &path)) {
        tracing::debug!(%error, "failed to write formula cache entry");
        let _ = fs::remove_file(temporary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(bytes: usize) -> FormulaLayoutRaster {
        FormulaLayoutRaster {
            raster: crate::formula_render::FormulaRaster {
                png: Arc::from(vec![0; bytes]),
                pixel_width: 2,
                pixel_height: 2,
            },
            columns: 1,
            rows: 1,
            is_block: false,
        }
    }

    #[test]
    fn formula_memory_cache_evicts_lru_entries_by_png_bytes() {
        let mut cache = FormulaMemoryCache::new(5);
        cache.put([1; 32], layout(3));
        cache.put([2; 32], layout(3));

        assert!(cache.get(&[1; 32]).is_none());
        assert!(cache.get(&[2; 32]).is_some());
        assert_eq!(cache.bytes, 3);

        cache.put([3; 32], layout(6));
        assert!(cache.get(&[3; 32]).is_none());
        assert!(cache.get(&[2; 32]).is_some());
        assert_eq!(cache.bytes, 3);
    }

    #[test]
    fn deactivating_formulas_discards_pending_generation() {
        let state = FormulaMessageState::new("$x$");
        let key = FormulaRenderKey {
            width: 80,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            foreground_rgb: [255; 3],
        };
        *state
            .active_key
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(key);
        state
            .variants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, RenderVariant::Pending);

        state.deactivate();

        assert!(
            state
                .variants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        assert_eq!(
            *state
                .active_key
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            None
        );
    }

    #[test]
    fn terminal_foreground_changes_formula_cache_key() {
        let source = FormulaSource {
            source_range: 0..3,
            body: "x".to_string(),
            kind: FormulaKind::Inline,
        };
        let light = FormulaRenderKey {
            width: 80,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            foreground_rgb: [255; 3],
        };
        let dark = FormulaRenderKey {
            foreground_rgb: [20; 3],
            ..light
        };

        assert_ne!(
            formula_cache_key(&source, dark),
            formula_cache_key(&source, light)
        );
    }
}
