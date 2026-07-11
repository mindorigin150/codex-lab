use std::sync::Arc;

use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::OutputArtifactStatus;
use super::OutputArtifactStore;
use crate::config::ToolOutputSpillConfig;

fn test_store(temp_dir: &TempDir, max_artifact_bytes: u64) -> Arc<OutputArtifactStore> {
    let output_dir = AbsolutePathBuf::try_from(temp_dir.path().join("artifacts"))
        .expect("artifact root is absolute");
    OutputArtifactStore::new(
        ToolOutputSpillConfig {
            enabled: true,
            token_threshold: 2,
            preview_token_limit: 3,
            max_artifact_bytes,
            max_store_bytes: 1024,
            retention_days: 7,
            output_dir,
        },
        "thread-1",
    )
    .expect("enabled store")
}

#[tokio::test]
async fn creates_one_artifact_after_threshold_and_reports_completion() {
    let temp_dir = TempDir::new().expect("temp dir");
    let store = test_store(&temp_dir, 1024);
    let mut spool = store.spool();
    let handle = spool.handle();

    spool.push(b"12345678").await;
    assert_eq!(handle.snapshot().await, None);
    spool.push(b"9abc").await;
    spool.complete().await;

    let descriptor = handle.snapshot().await.expect("artifact descriptor");
    assert_eq!(descriptor.observed_bytes, 12);
    assert_eq!(descriptor.stored_bytes, 12);
    assert_eq!(descriptor.omitted_bytes, 0);
    assert_eq!(descriptor.complete, true);
    assert_eq!(descriptor.status, OutputArtifactStatus::Completed);
    assert_eq!(
        std::fs::read(&descriptor.path).expect("read artifact"),
        b"123456789abc"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&descriptor.path)
                .expect("artifact metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
        );
    }
}

#[tokio::test]
async fn cap_is_reported_without_losing_observed_byte_count() {
    let temp_dir = TempDir::new().expect("temp dir");
    let store = test_store(&temp_dir, 10);
    let mut spool = store.spool();
    let handle = spool.handle();

    spool.push(b"0123456789abcdef").await;
    spool.complete().await;

    let descriptor = handle.snapshot().await.expect("artifact descriptor");
    assert_eq!(descriptor.observed_bytes, 16);
    assert_eq!(descriptor.stored_bytes, 10);
    assert_eq!(descriptor.omitted_bytes, 6);
    assert_eq!(descriptor.complete, false);
    assert_eq!(descriptor.status, OutputArtifactStatus::Capped);
}

#[test]
fn rejects_thread_ids_that_are_not_single_path_components() {
    let temp_dir = TempDir::new().expect("temp dir");
    let output_dir = AbsolutePathBuf::try_from(temp_dir.path().join("artifacts"))
        .expect("artifact root is absolute");
    let config = ToolOutputSpillConfig {
        enabled: true,
        token_threshold: 2,
        preview_token_limit: 3,
        max_artifact_bytes: 1024,
        max_store_bytes: 1024,
        retention_days: 7,
        output_dir,
    };

    assert!(OutputArtifactStore::new(config, "../other").is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_a_symlinked_artifact_root() {
    use std::os::unix::fs::symlink;

    let temp_dir = TempDir::new().expect("temp dir");
    let target = temp_dir.path().join("target");
    std::fs::create_dir(&target).expect("create target");
    let root = temp_dir.path().join("artifacts");
    symlink(&target, &root).expect("create artifact root symlink");
    let store = OutputArtifactStore::new(
        ToolOutputSpillConfig {
            enabled: true,
            token_threshold: 0,
            preview_token_limit: 3,
            max_artifact_bytes: 1024,
            max_store_bytes: 1024,
            retention_days: 7,
            output_dir: AbsolutePathBuf::try_from(root).expect("absolute root"),
        },
        "thread-1",
    )
    .expect("enabled store");
    let mut spool = store.spool();
    let handle = spool.handle();

    spool.push(b"output").await;

    assert_eq!(handle.snapshot().await, None);
    assert_eq!(
        std::fs::read_dir(target).expect("target directory").count(),
        0
    );
}
