use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

pub struct SingleInstance {
    file: File,
}

impl SingleInstance {
    fn open(path: &Path) -> Result<File> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open lock file {}", path.display()))
    }

    /// Take the lock **exclusively**. Returns `Ok(None)` when any other
    /// holder — exclusive or shared — already has it.
    ///
    /// On the restore lock, exclusive means "a restore is running": only
    /// restores take it this way, which is what lets a capture that fails to
    /// take it *shared* report `restore in progress` and mean it.
    pub fn acquire(path: &Path) -> Result<Option<Self>> {
        let file = Self::open(path)?;

        // Newer std::fs::File gained inherent lock/unlock methods (stable
        // since 1.89) that share names with fs2::FileExt's methods and
        // shadow them in normal method-call syntax, tripping clippy's
        // incompatible_msrv lint against our crate's MSRV. Call the fs2
        // trait methods through fully-qualified syntax so resolution is
        // unambiguous and doesn't depend on the std feature at all.
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self { file })),
            Err(_) => Ok(None),
        }
    }

    /// Take the lock **shared**: any number of holders may share it, but it
    /// cannot be taken while an exclusive holder has it, and an exclusive
    /// acquire fails while it is held.
    ///
    /// Captures use this on the restore lock. Two captures no longer block
    /// each other there (they serialise on their own capture lock instead),
    /// while a restore — the only exclusive taker — still excludes them
    /// both ways. That distinction is the whole point: a capture that
    /// cannot take the shared lock genuinely *is* blocked by a restore, so
    /// reporting `RestoreInProgress` is now true rather than a guess.
    pub fn acquire_shared(path: &Path) -> Result<Option<Self>> {
        let file = Self::open(path)?;
        match fs2::FileExt::try_lock_shared(&file) {
            Ok(()) => Ok(Some(Self { file })),
            Err(_) => Ok(None),
        }
    }

    /// Like [`acquire`](Self::acquire), but waits up to `timeout` for a
    /// current holder to let go before giving up.
    ///
    /// The boot restore needs this. The lock is shared between restores *and*
    /// captures, and a tmux hook firing as the first terminal of the session
    /// opens takes it for a fraction of a second. `osm-restore.service` is
    /// `Type=oneshot` with no `Restart`, so a non-blocking acquire turns that
    /// momentary overlap into "the boot restore never ran" — and the snapshot
    /// it would have restored then ages out of retention.
    ///
    /// Polls rather than using a blocking `flock`, so the wait is bounded and
    /// a wedged holder cannot hang the unit forever.
    pub fn acquire_blocking(path: &Path, timeout: Duration) -> Result<Option<Self>> {
        // Backs off rather than polling at a fixed interval. The database
        // schema lock is taken by *every* `osm` process on *every* open and
        // released again microseconds later, so a flat 25 ms poll turned a
        // burst of parallel tmux hooks into 25 ms of sleeping per waiter per
        // turn — a queue of thirty-two costing the better part of a second to
        // do nothing. Starting at 1 ms keeps the common short wait short; the
        // ceiling keeps a genuinely long wait (a restore holding the lock for
        // its whole run) from spinning.
        const FIRST_POLL: Duration = Duration::from_millis(1);
        const MAX_POLL: Duration = Duration::from_millis(25);
        let deadline = Instant::now() + timeout;
        let mut poll = FIRST_POLL;
        loop {
            if let Some(guard) = Self::acquire(path)? {
                return Ok(Some(guard));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            std::thread::sleep(poll.min(remaining));
            poll = (poll * 2).min(MAX_POLL);
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}
