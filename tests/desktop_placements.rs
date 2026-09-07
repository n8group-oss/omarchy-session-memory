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
    assert!(
        topo.placements.is_unknown(),
        "placement read from server {second} was attached to server {first}'s \
         topology: {:?}",
        topo.placements
    );
}

// ---------------------------------------------------------------------------
// The bounded retry: a compositor that stutters is asked again.
// ---------------------------------------------------------------------------

/// A compositor whose first `fail_first` client reads fail and whose later
/// ones answer, so a test can tell a *transient* failure from a dead one.
struct Flaky {
    remaining_failures: std::cell::Cell<u32>,
    calls: std::cell::Cell<u32>,
    clients: String,
}

impl Flaky {
    fn new(fail_first: u32, clients: &str) -> Self {
        Flaky {
            remaining_failures: std::cell::Cell::new(fail_first),
            calls: std::cell::Cell::new(0),
            clients: clients.to_string(),
        }
    }
}

impl HyprCtl for Flaky {
    fn clients_json(&self, _budget: std::time::Duration) -> anyhow::Result<String> {
        self.calls.set(self.calls.get() + 1);
        let left = self.remaining_failures.get();
        if left > 0 {
            self.remaining_failures.set(left - 1);
            anyhow::bail!("could not connect to the Hyprland socket");
        }
        Ok(self.clients.clone())
    }
    fn monitors_json(&self, _budget: std::time::Duration) -> anyhow::Result<String> {
        Ok(MONITORS.to_string())
    }
    fn dispatch(&self, _lua: &str, _budget: std::time::Duration) -> anyhow::Result<String> {
        panic!("no test here may dispatch: it would move a real window")
    }
}

/// The instant that made the maintainer lose 37% of an hour's captures.
///
/// `hyprctl` answered in 5ms throughout and a standalone reproduction of the
/// read succeeded 6 times out of 6, so what fails is not a dead compositor —
/// it is one of the transient branches, hit in the instant it has no answer.
/// Giving up on the first refusal turns that instant into a snapshot with no
/// placement in it. Asking again, within a bound, turns it into nothing at
/// all.
#[test]
fn a_compositor_that_stutters_once_is_asked_again_rather_than_written_off() {
    let t = server("stutter");
    let f = Flaky::new(1, A_BROWSER);
    let deadline = std::time::Instant::now() + desktop::PLACEMENT_READ_BUDGET;

    match desktop::placements_within(&f, &t, deadline).unwrap() {
        desktop::PlacementRead::Mapped(_, ps) => assert!(ps.is_empty(), "{ps:?}"),
        other => panic!("one failed call was treated as an unreadable desktop: {other:?}"),
    }
    assert_eq!(
        f.calls.get(),
        2,
        "the read was not retried, or was retried more than it needed to be"
    );
}

/// And the retry is a bound, not a loop.
///
/// A compositor that has really gone away must not hold a capture open. The
/// budget is what separates "wait out an instant" from "wait for a machine
/// that is not coming back", and a retry with no ceiling would park the
/// daemon's two-minute tick inside a dead `hyprctl`.
#[test]
fn a_compositor_that_never_answers_gives_up_inside_the_budget() {
    let t = server("givesup");
    let f = Flaky::new(u32::MAX, A_BROWSER);

    let started = std::time::Instant::now();
    let read =
        desktop::placements_within(&f, &t, started + desktop::PLACEMENT_READ_BUDGET).unwrap();
    let elapsed = started.elapsed();

    match read {
        desktop::PlacementRead::Unreadable(why) => assert!(
            why.contains("Hyprland"),
            "the reason must name what actually went wrong: {why}"
        ),
        other => panic!("a compositor that never answered produced {other:?}"),
    }
    assert!(
        elapsed < desktop::PLACEMENT_READ_BUDGET * 2,
        "the retry ran for {elapsed:?}, past its {:?} budget",
        desktop::PLACEMENT_READ_BUDGET
    );
    assert!(
        f.calls.get() > 1,
        "nothing was retried at all: {} call(s)",
        f.calls.get()
    );
}

/// The retry never papers over the incarnation check.
///
/// Topology and placement must still come from one tmux incarnation. A read
/// that is retried until it succeeds is still a read of whatever server is
/// there *now*, and if that is not the server the topology came from, the two
/// describe different machines and must not be stored together.
#[test]
fn the_retry_does_not_relax_the_incarnation_check() {
    let t = server("retryidentity");
    let mut topo = osm::capture::collect(&t).unwrap();
    // A topology attributed to a server that is not the one running here.
    topo.server = Some("deadbeefdeadbeef".to_string());

    let f = Flaky::new(1, A_BROWSER);
    osm::capture::attach_placements(&mut topo, &t, &f).unwrap();

    assert!(
        topo.placements.is_unknown(),
        "a retried read was attached to a topology from another server: {:?}",
        topo.placements
    );
}

/// And a topology that belongs to no tmux server is not made to wait out the
/// budget.
///
/// There is no session for a window to hold, so there is nothing the retry
/// could discover; spending the whole budget on it would put three seconds
/// into a capture that has nothing to place. Built by hand rather than by
/// `collect`, which cannot produce this shape — it fails outright when the
/// server it was pointed at is not there — while `Topology` can hold it and
/// `write_topology_in` has a branch for it.
#[test]
fn a_topology_with_no_server_is_answered_at_once_rather_than_waited_out() {
    let t = Tmux::with_socket(&socket("noserver-fast"));
    assert!(!t.server_running(), "this socket must have no server on it");
    let mut topo = osm::capture::Topology {
        placements: osm::desktop::Placements::Off,
        sessions: Vec::new(),
        windows: Vec::new(),
        panes: Vec::new(),
        server: None,
        server_at_end: None,
    };

    let f = Flaky::new(0, A_BROWSER);
    let started = std::time::Instant::now();
    osm::capture::attach_placements(&mut topo, &t, &f).unwrap();
    let elapsed = started.elapsed();

    assert!(topo.placements.is_unknown(), "{:?}", topo.placements);
    assert_eq!(
        f.calls.get(),
        0,
        "the compositor was asked about a machine with no tmux server on it"
    );
    assert!(
        elapsed < desktop::PLACEMENT_READ_BUDGET,
        "waited {elapsed:?} for a server that is not there"
    );
}

/// A compositor that takes the time it is given, and remembers how much that
/// was.
///
/// `Flaky` above answers instantly and ignores its `budget`, which is why the
/// six-second assertion in
/// [`a_compositor_that_never_answers_gives_up_inside_the_budget`] cannot see
/// this defect: a stub that never waits makes any budget look the same. This
/// one behaves the way `hypr::run_hyprctl` does — it runs until it is either
/// finished or out of budget — so what the read hands it decides how long the
/// capture is held open.
struct Timed {
    /// Every budget handed to the compositor, in call order.
    budgets: std::cell::RefCell<Vec<std::time::Duration>>,
    /// How long the client list takes to come back.
    clients_take: std::time::Duration,
    /// How long the monitor list runs before giving up — capped so that a
    /// failing run of this test finishes in seconds rather than in the
    /// fifteen it is complaining about.
    monitors_cap: std::time::Duration,
}

impl HyprCtl for Timed {
    fn clients_json(&self, budget: std::time::Duration) -> anyhow::Result<String> {
        self.budgets.borrow_mut().push(budget);
        std::thread::sleep(self.clients_take.min(budget));
        Ok(A_BROWSER.to_string())
    }
    fn monitors_json(&self, budget: std::time::Duration) -> anyhow::Result<String> {
        self.budgets.borrow_mut().push(budget);
        std::thread::sleep(budget.min(self.monitors_cap));
        anyhow::bail!("could not connect to the Hyprland socket")
    }
    fn dispatch(&self, _lua: &str, _budget: std::time::Duration) -> anyhow::Result<String> {
        panic!("no test here may dispatch: it would move a real window")
    }
}

/// The retry budget bounds the calls it makes, not just how many of them
/// there are.
///
/// `PLACEMENT_READ_BUDGET` is three seconds, and the reason it is three is
/// that a capture is held open for the whole of it — `attach_placements` runs
/// inside one. Handing each `hyprctl` call the independent fifteen-second
/// `CALL_TIMEOUT` made that bound a statement about nothing: an attempt
/// starting a millisecond before the deadline still ran two calls of up to
/// fifteen seconds each, so a compositor that had wedged rather than died
/// held the capture for thirty seconds against a budget of three.
///
/// So each call is started with what is left of the budget, recomputed
/// between them — the same rule `spawn_and_place` already follows through
/// `call_budget`, which is also where the floor comes from: a deadline may
/// bound how long osm waits, but it may not cut a call off after a
/// millisecond and call the placement failed for nothing but arithmetic.
#[test]
fn each_compositor_call_is_started_with_what_is_left_of_the_budget() {
    let t = server("callbudget");
    let h = Timed {
        budgets: std::cell::RefCell::new(Vec::new()),
        clients_take: std::time::Duration::from_millis(1200),
        monitors_cap: std::time::Duration::from_secs(4),
    };

    let started = std::time::Instant::now();
    let read =
        desktop::placements_within(&h, &t, started + desktop::PLACEMENT_READ_BUDGET).unwrap();
    let elapsed = started.elapsed();

    assert!(
        matches!(read, desktop::PlacementRead::Unreadable(_)),
        "the monitor list never came back, so this is not a readable desktop: {read:?}"
    );

    let budgets = h.budgets.borrow().clone();
    assert!(
        budgets.len() >= 2,
        "both halves of the read have to be asked before this can say anything: {budgets:?}"
    );
    assert!(
        budgets.iter().all(|b| *b <= desktop::PLACEMENT_READ_BUDGET),
        "a call was started with more time than the whole retry budget: {budgets:?}"
    );
    assert!(
        budgets[1] < budgets[0],
        "the second call was not given the budget the first one had spent \
         ({:?} then {:?}); it is recomputed between them or it bounds nothing",
        budgets[0],
        budgets[1]
    );
    assert!(
        elapsed < desktop::PLACEMENT_READ_BUDGET * 2,
        "the read held the capture open for {elapsed:?} against a {:?} budget",
        desktop::PLACEMENT_READ_BUDGET
    );
}
