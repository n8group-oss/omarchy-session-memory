//! Collecting placement, and the distinction that protects a good layout.
//!
//! Every server here is private, named by a `-L` socket carrying this
//! process's id, and shut down at the end of the test that started it.

mod common;

use anyhow::Result;
use osm::desktop;
use osm::hypr::HyprCtl;
use osm::tmux::Tmux;

/// A compositor whose replies the test controls.
struct Fake {
    clients: Result<String, String>,
    monitors: Result<String, String>,
}

impl HyprCtl for Fake {
    fn clients_json(&self, _budget: std::time::Duration) -> anyhow::Result<String> {
        self.clients.clone().map_err(|e| anyhow::anyhow!(e))
    }
    fn monitors_json(&self, _budget: std::time::Duration) -> anyhow::Result<String> {
        self.monitors.clone().map_err(|e| anyhow::anyhow!(e))
    }
    fn dispatch(&self, _lua: &str, _budget: std::time::Duration) -> anyhow::Result<String> {
        panic!("no test here may dispatch: it would move a real window")
    }
}

const MONITORS: &str = r#"[{"id":0,"name":"DP-1","description":"GWD ARZOPA 111",
  "x":0,"y":0,"width":1920,"height":1080,"scale":1.0,"transform":0,"focused":true}]"#;

/// A window that is not a terminal with a session in it.
const A_BROWSER: &str = r#"[{"address":"0x1","pid":999999,"class":"chromium","title":"x",
    "workspace":{"id":5,"name":"5"},"monitor":0,"at":[0,0],"size":[10,10],
    "floating":false}]"#;

fn socket(label: &str) -> String {
    format!("osm-plc-{label}-{}", std::process::id())
}

/// A private tmux server with one detached session and no attached client.
///
/// Every test needs a *running* server, because "no tmux server" is itself
/// one of the unreadable states this module refuses to write down as an empty
/// layout — see [`a_tmux_that_cannot_be_read_yields_none_not_an_empty_layout`].
///
/// Shut down from [`Drop`], not from a line at the end of the test: an
/// assertion that fires early would otherwise leave a live server and its
/// socket behind in the directory the developer's own tmux lives in.
struct Server(Tmux);

impl std::ops::Deref for Server {
    type Target = Tmux;
    fn deref(&self) -> &Tmux {
        &self.0
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn server(label: &str) -> Server {
    let t = Tmux::with_socket(&socket(label));
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    Server(t)
}

#[test]
fn an_unreachable_compositor_yields_none_not_an_empty_layout() {
    // The distinction this whole module exists for. `None` means "do not
    // write placement"; `Some(vec![])` means "the desktop really is empty".
    // Collapsing them is how a one-session layout overwrote a seven-session
    // one, unrecoverably.
    let t = server("unreachable");
    let f = Fake {
        clients: Err("compositor gone".into()),
        monitors: Ok(MONITORS.into()),
    };
    assert_eq!(desktop::placements(&f, &t).unwrap(), None);
}

#[test]
fn a_malformed_reply_is_unreachable_not_empty() {
    let t = server("malformed");
    for bad in ["{}", "null", "", "not json"] {
        let f = Fake {
            clients: Ok(bad.into()),
            monitors: Ok(MONITORS.into()),
        };
        assert_eq!(
            desktop::placements(&f, &t).unwrap(),
            None,
            "{bad:?} was read as an empty desktop"
        );
    }
}

#[test]
fn a_tmux_that_cannot_be_read_yields_none_not_an_empty_layout() {
    // The half that was missing. With no server on this socket, `list-clients`
    // fails; that failure used to be flattened to `""`, parsed as zero
    // clients, and returned as `Some(vec![])` — a confident statement that no
    // session has a terminal window, written over the last good layout. The
    // compositor here answers perfectly well and reports a window, so the
    // only thing that can produce `None` is the tmux side being unreadable.
    let t = Tmux::with_socket(&socket("noserver"));
    assert!(!t.server_running(), "this socket must have no server on it");
    let f = Fake {
        clients: Ok(A_BROWSER.into()),
        monitors: Ok(MONITORS.into()),
    };
    assert_eq!(desktop::placements(&f, &t).unwrap(), None);
}

#[test]
fn a_server_replaced_during_collection_yields_none() {
    // A tmux restart between the window list and the client list produces a
    // mapping across two servers: the sessions the second server's clients
    // name are not the sessions the first one held. Nothing about that is
    // worth writing down, and writing it down as "these windows hold no
    // session" is worse.
    //
    // The replacement happens *inside* the compositor call, which is the one
    // seam a test can drive: it sits between the two identity reads.
    struct ReplacesTheServer {
        socket: String,
    }
    impl HyprCtl for ReplacesTheServer {
        fn clients_json(&self, _budget: std::time::Duration) -> anyhow::Result<String> {
            let t = Tmux::with_socket(&self.socket);
            t.run(&["kill-server"]).expect("the first server dies");
            // Retried: for a moment after `kill-server` the socket is still
            // there and tmux answers "server exited unexpectedly" instead of
            // starting a new one. Letting that error escape would abort the
            // collection here, and the test would then pass because the
            // *compositor* failed rather than because the server moved —
            // which is what it did before this loop existed.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if t.run(&["new-session", "-d", "-s", "beta", "-c", "/tmp"])
                    .is_ok()
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "a replacement server never started"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(A_BROWSER.to_string())
        }
        fn monitors_json(&self, _budget: std::time::Duration) -> anyhow::Result<String> {
            Ok(MONITORS.to_string())
        }
        fn dispatch(&self, _lua: &str, _budget: std::time::Duration) -> anyhow::Result<String> {
            panic!("no test here may dispatch: it would move a real window")
        }
    }

    let t = server("replaced");
    let h = ReplacesTheServer {
        socket: socket("replaced"),
    };
    assert_eq!(desktop::placements(&h, &t).unwrap(), None);
}

#[test]
fn a_client_line_that_does_not_parse_is_an_error_not_a_dropped_line() {
    // Silently dropping an unparseable line shortens the client list, and a
    // shorter client list is indistinguishable from "those terminals are not
    // there" — which is what gets written down.
    assert!(desktop::parse_clients_output("1234 dev\nnot-a-pid notes\n").is_err());
    assert!(desktop::parse_clients_output("nosession\n").is_err());
    // A blank line is the whole of tmux's output when nothing is attached.
    assert_eq!(desktop::parse_clients_output("\n").unwrap(), vec![]);
}

#[test]
fn a_genuinely_empty_desktop_is_some_empty() {
    let t = server("empty");
    let f = Fake {
        clients: Ok("[]".into()),
        monitors: Ok(MONITORS.into()),
    };
    assert_eq!(desktop::placements(&f, &t).unwrap(), Some(vec![]));
}

#[test]
fn a_window_with_no_tmux_client_inside_contributes_no_placement() {
    // A browser is a window on a workspace; it is not a session's terminal.
    let t = server("browser");
    let f = Fake {
        clients: Ok(A_BROWSER.into()),
        monitors: Ok(MONITORS.into()),
    };
    assert_eq!(desktop::placements(&f, &t).unwrap(), Some(vec![]));
}

#[test]
fn the_terminal_kind_is_derived_from_the_window_class() {
    assert_eq!(
        desktop::terminal_kind_of("com.mitchellh.ghostty"),
        "ghostty"
    );
    assert_eq!(desktop::terminal_kind_of("Alacritty"), "alacritty");
    assert_eq!(desktop::terminal_kind_of("foot"), "foot");
    assert_eq!(desktop::terminal_kind_of("kitty"), "kitty");
    // An unknown class is preserved rather than guessed at.
    assert_eq!(
        desktop::terminal_kind_of("org.wezfurlong"),
        "org.wezfurlong"
    );
}

#[test]
fn placement_read_from_a_replacement_server_is_not_attached_to_the_first_ones_topology() {
    // The gap the per-collection identity checks cannot close. Server A
    // answers the topology and dies; server B is up before the placement pass
    // begins, so B satisfies *both* identity reads inside
    // `placements_with_incarnation` and the mapping it produces looks
    // perfectly consistent — with itself. Attaching it to A's topology stores
    // one server's sessions beside another server's windows and marks the
    // result `complete`.
    //
    // The compositor here answers correctly throughout: the only thing wrong
    // is which tmux server the two halves came from.
    let t = server("crossserver");
    let topo = osm::capture::collect(&t).unwrap();
    let first = topo
        .server
        .clone()
        .expect("the first server identifies itself");

    // A dies, B takes its place — between the topology and the placement, the
    // one window no test could otherwise reach.
    t.run(&["kill-server"]).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if t.run(&["new-session", "-d", "-s", "beta", "-c", "/tmp"])
            .is_ok()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a replacement server never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let second = t
        .running_server_incarnation()
        .unwrap()
        .expect("the replacement identifies itself");
    assert_ne!(first, second, "the server was not actually replaced");

    let f = Fake {
        clients: Ok(A_BROWSER.into()),
        monitors: Ok(MONITORS.into()),
    };
    let mut topo = topo;
    osm::capture::attach_placements(&mut topo, &t, &f).unwrap();
    assert_eq!(
        topo.placements, None,
        "placement read from server {second} was attached to server {first}'s topology"
    );
}
