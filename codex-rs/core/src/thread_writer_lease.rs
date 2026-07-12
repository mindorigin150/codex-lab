use fs2::FileExt;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::ThreadId;

#[derive(Debug)]
pub(crate) enum ThreadWriterLeaseError {
    AlreadyActive { owner: Option<String> },
    Io(io::Error),
}

impl std::fmt::Display for ThreadWriterLeaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyActive { owner: Some(owner) } => {
                write!(
                    formatter,
                    "thread is active in another Codex process ({owner})"
                )
            }
            Self::AlreadyActive { owner: None } => {
                formatter.write_str("thread is active in another Codex process")
            }
            Self::Io(error) => write!(formatter, "thread writer lock failed: {error}"),
        }
    }
}

impl std::error::Error for ThreadWriterLeaseError {}

/// An exclusive, process-scoped writer lease for one persisted thread.
///
/// The operating system releases the advisory lock when the process exits,
/// including after an ungraceful termination. The small metadata payload is
/// diagnostic only; ownership is determined exclusively by the file lock.
#[derive(Debug)]
pub(crate) struct ThreadWriterLease {
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

impl ThreadWriterLease {
    pub(crate) fn acquire(
        rollout_path: &Path,
        thread_id: ThreadId,
    ) -> Result<Self, ThreadWriterLeaseError> {
        let lock_dir = rollout_path.parent().ok_or_else(|| {
            ThreadWriterLeaseError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rollout path has no parent directory",
            ))
        })?;
        std::fs::create_dir_all(lock_dir).map_err(ThreadWriterLeaseError::Io)?;
        // Keep the lease beside the rollout so installations with different CODEX_HOME values
        // but shared conversation storage still coordinate on the same inode.
        let path = lock_dir.join(format!(".{thread_id}.writer.lock"));
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(ThreadWriterLeaseError::Io)?;

        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let owner = read_owner_metadata(&mut file);
                return Err(ThreadWriterLeaseError::AlreadyActive { owner });
            }
            Err(error) => return Err(ThreadWriterLeaseError::Io(error)),
        }

        if let Err(error) = write_owner_metadata(&mut file, thread_id) {
            let _ = FileExt::unlock(&file);
            return Err(ThreadWriterLeaseError::Io(error));
        }

        Ok(Self { file, path })
    }
}

impl Drop for ThreadWriterLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn read_owner_metadata(file: &mut File) -> Option<String> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut owner = String::new();
    file.take(1024).read_to_string(&mut owner).ok()?;
    let owner = owner.trim();
    (!owner.is_empty()).then(|| owner.to_string())
}

fn write_owner_metadata(file: &mut File, thread_id: ThreadId) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "pid={}, thread_id={thread_id}", std::process::id())?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_writer_can_hold_a_thread_lease() {
        let codex_home = tempfile::tempdir().expect("temporary codex home");
        let thread_id = ThreadId::new();
        let rollout_path = codex_home.path().join("rollout.jsonl");
        let first =
            ThreadWriterLease::acquire(&rollout_path, thread_id).expect("first writer lease");

        let second = ThreadWriterLease::acquire(&rollout_path, thread_id);
        assert!(matches!(
            second,
            Err(ThreadWriterLeaseError::AlreadyActive { .. })
        ));

        drop(first);
        ThreadWriterLease::acquire(&rollout_path, thread_id)
            .expect("lease should be released when its owner drops");
    }

    #[test]
    fn different_threads_have_independent_writer_leases() {
        let codex_home = tempfile::tempdir().expect("temporary codex home");
        let rollout_path = codex_home.path().join("rollout.jsonl");
        let _first =
            ThreadWriterLease::acquire(&rollout_path, ThreadId::new()).expect("first writer lease");
        let _second = ThreadWriterLease::acquire(&rollout_path, ThreadId::new())
            .expect("second writer lease");
    }
}
