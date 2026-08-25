mod common;

use osm::tmux::Tmux;

struct Server(Tmux, #[allow(dead_code)] String);

impl Server {
    fn start(label: &str) -> Self {
        let sock = format!("osm-test-{}-{}", label, std::process::id());
        let t = Tmux::with_socket(&sock);
        t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
            .expect("start server");
        Server(t, sock)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

#[test]
fn reads_real_topology() {
    let s = Server::start("read");
    let t = &s.0;
    t.run(&["new-window", "-t", "alpha", "-n", "logs", "-c", "/tmp"])
        .unwrap();
    t.run(&["split-window", "-t", "alpha:logs", "-c", "/tmp"])
        .unwrap();

    let sessions = t.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].name, "alpha");

    let windows = t.list_windows().unwrap();
    assert_eq!(windows.len(), 2);
    assert!(windows.iter().any(|w| w.name == "logs"));
    assert!(windows.iter().all(|w| !w.layout.is_empty()));
    assert_eq!(windows.iter().filter(|w| w.active).count(), 1);

    let panes = t.list_panes().unwrap();
    assert_eq!(panes.len(), 3);
    assert!(panes.iter().all(|p| p.cwd == "/tmp"));
}

#[test]
fn server_running_is_false_for_unused_socket() {
    let t = Tmux::with_socket(&format!("osm-test-absent-{}", std::process::id()));
    assert!(!t.server_running());
}
