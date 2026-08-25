//! Restore-vs-capture lock contention.
//!
//! The existing suites cover capture-vs-lock (`capture_guard.rs`) and
//! restore-vs-lock as a single-holder invariant (`restore_attempt.rs`), but
//! never the pairing that actually bites at boot: a tmux `session-created`
//! hook takes the shared restore lock for a fraction of a second exactly as
//! `osm-restore.service` starts.

use osm::{capture, lock::SingleInstance};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const HELD_FOR: Duration = Duration::from_millis(400);

#[test]
fn a_restore_waits_out_a_capture_that_holds_the_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join("restore.lock");

    let (holding_tx, holding_rx) = mpsc::channel();
    let capture_path = lock_path.clone();
    let capturing = std::thread::spawn(move || {
        capture::with_restore_lock(&capture_path, || {
            holding_tx.send(()).unwrap();
            std::thread::sleep(HELD_FOR);
            Ok(())
        })
        .unwrap()
        .expect("the capture must be the one holding the lock");
    });

    holding_rx.recv().unwrap();
    let started = Instant::now();
    let guard = SingleInstance::acquire_blocking(&lock_path, Duration::from_secs(30)).unwrap();
    let waited = started.elapsed();

    assert!(
        guard.is_some(),
        "a restore must not give up because a capture briefly held the lock"
    );
    assert!(
        waited >= HELD_FOR / 2,
        "the restore should have waited for the capture to finish, but returned after {waited:?}"
    );

    drop(guard);
    capturing.join().unwrap();
}

#[test]
fn the_wait_is_bounded_so_a_wedged_holder_cannot_hang_the_unit() {
    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join("restore.lock");
    let _wedged = SingleInstance::acquire(&lock_path).unwrap().unwrap();

    let started = Instant::now();
    let guard = SingleInstance::acquire_blocking(&lock_path, Duration::from_millis(300)).unwrap();
    let waited = started.elapsed();

    assert!(
        guard.is_none(),
        "a permanently held lock must still time out"
    );
    assert!(
        waited >= Duration::from_millis(250),
        "gave up after only {waited:?}; the timeout was not honoured"
    );
}

#[test]
fn a_free_lock_is_acquired_without_waiting() {
    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join("restore.lock");

    let started = Instant::now();
    let guard = SingleInstance::acquire_blocking(&lock_path, Duration::from_secs(30)).unwrap();
    assert!(guard.is_some());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "an uncontended acquire must not sleep"
    );
}
