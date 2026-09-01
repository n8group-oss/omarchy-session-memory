mod common;

use osm::{capture, db, lock::SingleInstance, tmux::Tmux};

struct Server(Tmux);

impl Server {
    fn start(label: &str) -> Self {
        let t = Tmux::with_socket(&format!("osm-guard-{}-{}", label, std::process::id()));
        t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
            .unwrap();
        Server(t)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

#[test]
fn capture_is_refused_while_restore_holds_the_lock() {
    let s = Server::start("held");
    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join("restore.lock");
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();

    let _restore_guard = SingleInstance::acquire(&lock_path).unwrap().unwrap();

    let result = capture::snapshot_guarded(&mut conn, &s.0, None, "hook", &lock_path).unwrap();
    assert!(result.is_none(), "capture must be refused during restore");

    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "no snapshot row may be written");
}

#[test]
fn capture_proceeds_once_the_lock_is_free() {
    let s = Server::start("free");
    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join("restore.lock");
    let mut conn = db::open(&tmp.path().join("state.db")).unwrap();

    let guard = SingleInstance::acquire(&lock_path).unwrap().unwrap();
    drop(guard);

    let result = capture::snapshot_guarded(&mut conn, &s.0, None, "hook", &lock_path).unwrap();
    assert!(result.is_some());
}

#[test]
fn lock_is_held_for_the_duration_of_the_guarded_operation() {
    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join("restore.lock");

    capture::with_restore_lock(&lock_path, || {
        // While inside the closure, the lock should be held.
        // A second acquire should fail.
        let result = SingleInstance::acquire(&lock_path).unwrap();
        assert!(
            result.is_none(),
            "lock must be held while the guarded operation is in flight"
        );
        Ok(())
    })
    .unwrap();
}
