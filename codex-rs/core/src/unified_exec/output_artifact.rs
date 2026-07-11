use std::fs::DirBuilder;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;

use serde::Serialize;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;
use tracing::warn;
use uuid::Uuid;

use crate::config::ToolOutputSpillConfig;

const MAX_PENDING_OUTPUT_BYTES: usize = 1024 * 1024;

static ROOT_QUOTAS: OnceLock<StdMutex<std::collections::HashMap<PathBuf, Weak<RootQuota>>>> =
    OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutputArtifactStatus {
    Running,
    Completed,
    Capped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct OutputArtifactDescriptor {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) observed_bytes: u64,
    pub(crate) stored_bytes: u64,
    pub(crate) omitted_bytes: u64,
    pub(crate) complete: bool,
    pub(crate) status: OutputArtifactStatus,
    #[serde(skip)]
    pub(crate) preview_token_limit: usize,
}

#[derive(Debug)]
pub(crate) struct OutputArtifactStore {
    config: ToolOutputSpillConfig,
    thread_dir: PathBuf,
    initialized: OnceCell<()>,
    root_quota: Arc<RootQuota>,
}

#[derive(Debug)]
struct RootQuota {
    root: PathBuf,
    initialized_bytes: OnceCell<u64>,
    stored_bytes: AtomicU64,
}

impl RootQuota {
    fn shared(root: PathBuf) -> Arc<Self> {
        let quotas = ROOT_QUOTAS.get_or_init(|| StdMutex::new(std::collections::HashMap::new()));
        let mut quotas = quotas
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(quota) = quotas.get(&root).and_then(Weak::upgrade) {
            return quota;
        }
        let quota = Arc::new(Self {
            root: root.clone(),
            initialized_bytes: OnceCell::new(),
            stored_bytes: AtomicU64::new(0),
        });
        quotas.insert(root, Arc::downgrade(&quota));
        quota
    }

    async fn initialize(&self, retention: Duration) -> std::io::Result<()> {
        let root = self.root.clone();
        self.initialized_bytes
            .get_or_try_init(|| async {
                let bytes = tokio::task::spawn_blocking(move || {
                    create_private_dir(&root)?;
                    cleanup_and_measure(&root, retention)
                })
                .await
                .map_err(std::io::Error::other)??;
                self.stored_bytes.store(bytes, Ordering::Release);
                Ok::<u64, std::io::Error>(bytes)
            })
            .await?;
        Ok(())
    }

    fn reserve(&self, requested: u64, max_store_bytes: u64) -> u64 {
        let mut current = self.stored_bytes.load(Ordering::Acquire);
        loop {
            let available = max_store_bytes.saturating_sub(current);
            let reserved = requested.min(available);
            match self.stored_bytes.compare_exchange_weak(
                current,
                current.saturating_add(reserved),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return reserved,
                Err(actual) => current = actual,
            }
        }
    }
}

impl OutputArtifactStore {
    pub(crate) fn new(config: ToolOutputSpillConfig, thread_id: &str) -> Option<Arc<Self>> {
        (config.enabled && safe_path_component(thread_id)).then(|| {
            let root = config.output_dir.as_path().to_path_buf();
            Arc::new(Self {
                thread_dir: config.output_dir.as_path().join(thread_id),
                root_quota: RootQuota::shared(root),
                config,
                initialized: OnceCell::new(),
            })
        })
    }

    pub(crate) fn spool(self: &Arc<Self>) -> OutputArtifactSpool {
        OutputArtifactSpool {
            store: Arc::clone(self),
            descriptor: OutputArtifactHandle::default(),
            id: Uuid::new_v4().to_string(),
            path: None,
            file: None,
            pending: Vec::new(),
            observed_bytes: 0,
            stored_bytes: 0,
            capped: false,
            failed: false,
            completed: false,
        }
    }

    async fn initialize(&self) -> std::io::Result<()> {
        let thread_dir = self.thread_dir.clone();
        let retention = Duration::from_secs(self.config.retention_days.saturating_mul(86_400));
        self.initialized
            .get_or_try_init(|| async {
                self.root_quota.initialize(retention).await?;
                tokio::task::spawn_blocking(move || {
                    create_private_dir(&thread_dir)?;
                    Ok::<(), std::io::Error>(())
                })
                .await
                .map_err(std::io::Error::other)??;
                Ok::<(), std::io::Error>(())
            })
            .await?;
        Ok(())
    }

    fn reserve(&self, requested: u64) -> u64 {
        self.root_quota
            .reserve(requested, self.config.max_store_bytes)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OutputArtifactHandle {
    descriptor: Arc<Mutex<Option<OutputArtifactDescriptor>>>,
}

impl OutputArtifactHandle {
    pub(crate) async fn snapshot(&self) -> Option<OutputArtifactDescriptor> {
        self.descriptor.lock().await.clone()
    }
}

#[derive(Debug)]
pub(crate) struct OutputArtifactSpool {
    store: Arc<OutputArtifactStore>,
    descriptor: OutputArtifactHandle,
    id: String,
    path: Option<PathBuf>,
    file: Option<File>,
    pending: Vec<u8>,
    observed_bytes: u64,
    stored_bytes: u64,
    capped: bool,
    failed: bool,
    completed: bool,
}

impl OutputArtifactSpool {
    pub(crate) fn handle(&self) -> OutputArtifactHandle {
        self.descriptor.clone()
    }

    pub(crate) async fn push(&mut self, chunk: &[u8]) {
        self.observed_bytes = self.observed_bytes.saturating_add(chunk.len() as u64);
        if self.failed || self.capped {
            self.publish().await;
            return;
        }

        if self.file.is_none() {
            let approximate_tokens = self.observed_bytes.saturating_add(3) / 4;
            let pending_would_reach_limit =
                self.pending.len().saturating_add(chunk.len()) >= MAX_PENDING_OUTPUT_BYTES;
            if approximate_tokens <= self.store.config.token_threshold as u64
                && !pending_would_reach_limit
            {
                self.pending.extend_from_slice(chunk);
                return;
            }
            if let Err(err) = self.open_and_flush_pending().await {
                self.failed = true;
                self.pending.clear();
                warn!("failed to create unified-exec output artifact: {err}");
            } else if !self.capped {
                self.write_bounded(chunk).await;
            }
            self.publish().await;
            return;
        }

        self.write_bounded(chunk).await;
        self.publish().await;
    }

    async fn open_and_flush_pending(&mut self) -> std::io::Result<()> {
        self.store.initialize().await?;
        let path = self.store.thread_dir.join(format!("{}.log", self.id));
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        self.file = Some(options.open(&path).await?);
        self.path = Some(path);
        let pending = std::mem::take(&mut self.pending);
        self.write_bounded(&pending).await;
        Ok(())
    }

    async fn write_bounded(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let artifact_available = self
            .store
            .config
            .max_artifact_bytes
            .saturating_sub(self.stored_bytes);
        let wanted = (bytes.len() as u64).min(artifact_available);
        let reserved = self.store.reserve(wanted);
        if reserved == 0 {
            self.capped = true;
            return;
        }
        let Some(file) = self.file.as_mut() else {
            self.failed = true;
            return;
        };
        if let Err(err) = file.write_all(&bytes[..reserved as usize]).await {
            self.failed = true;
            warn!("failed to append unified-exec output artifact: {err}");
            return;
        }
        self.stored_bytes = self.stored_bytes.saturating_add(reserved);
        if reserved < bytes.len() as u64
            || self.stored_bytes >= self.store.config.max_artifact_bytes
        {
            self.capped = true;
        }
    }

    pub(crate) async fn complete(&mut self) {
        if let Some(mut file) = self.file.take()
            && let Err(err) = file.flush().await
        {
            self.failed = true;
            warn!("failed to flush unified-exec output artifact: {err}");
        }
        self.completed = true;
        self.publish().await;
    }

    fn current_descriptor(&self) -> Option<OutputArtifactDescriptor> {
        let path = self.path.as_ref()?;
        let omitted_bytes = self.observed_bytes.saturating_sub(self.stored_bytes);
        let complete = self.completed && !self.capped && !self.failed && omitted_bytes == 0;
        let status = if self.failed {
            OutputArtifactStatus::Failed
        } else if self.capped {
            OutputArtifactStatus::Capped
        } else if !self.completed {
            OutputArtifactStatus::Running
        } else {
            OutputArtifactStatus::Completed
        };
        Some(OutputArtifactDescriptor {
            id: self.id.clone(),
            path: path.to_string_lossy().into_owned(),
            observed_bytes: self.observed_bytes,
            stored_bytes: self.stored_bytes,
            omitted_bytes,
            complete,
            status,
            preview_token_limit: self.store.config.preview_token_limit,
        })
    }

    async fn publish(&self) {
        *self.descriptor.descriptor.lock().await = self.current_descriptor();
    }
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    reject_symlink_components(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)?;
        reject_symlink_components(path)?;
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)?;
    Ok(())
}

fn reject_symlink_components(path: &Path) -> std::io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let Ok(metadata) = current.symlink_metadata() else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "artifact path contains a symlink: {}",
                current.display()
            )));
        }
        if !metadata.is_dir() && current != path {
            return Err(std::io::Error::other(format!(
                "artifact path component is not a directory: {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn safe_path_component(value: &str) -> bool {
    !value.is_empty() && Path::new(value).components().count() == 1 && value != "." && value != ".."
}

fn cleanup_and_measure(root: &Path, retention: Duration) -> std::io::Result<u64> {
    let now = SystemTime::now();
    let mut total = 0_u64;
    let Ok(thread_dirs) = std::fs::read_dir(root) else {
        return Ok(0);
    };
    for thread_dir in thread_dirs.flatten() {
        let Ok(file_type) = thread_dir.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(thread_dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let Ok(metadata) = file.path().symlink_metadata() else {
                continue;
            };
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                continue;
            }
            let expired = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > retention);
            if expired {
                let _ = std::fs::remove_file(file.path());
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
#[path = "output_artifact_tests.rs"]
mod tests;
