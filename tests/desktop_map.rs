use osm::desktop::{self, ClientPid};
use osm::hypr;

fn client(addr: &str, pid: u32) -> hypr::Client {
    hypr::Client {
        address: addr.to_string(),
        pid,
        class: "com.mitchellh.ghostty".to_string(),
        title: "omarchy:whatever".to_string(),
        workspace_id: 3,
        workspace_name: "3".to_string(),
        monitor_index: 1,
        at: (0, 0),
        size: (100, 100),
        floating: false,
    }
}

#[test]
fn parses_tmux_client_pids_and_sessions() {
    let out = "1234 dev\n5678 notes\n";
    let cs = desktop::parse_clients_output(out).unwrap();
    assert_eq!(cs.len(), 2);
    assert_eq!(
        cs[0],
        ClientPid {
            pid: 1234,
            session: "dev".to_string()
        }
    );
    assert_eq!(cs[1].session, "notes");
}

#[test]
fn a_session_name_containing_a_space_survives() {
    let cs = desktop::parse_clients_output("42 my session\n").unwrap();
    assert_eq!(cs[0].session, "my session");
}

#[test]
fn ancestors_start_at_the_pid_and_terminate() {
    let a = desktop::ancestors(std::process::id());
    assert_eq!(
        a[0],
        std::process::id(),
        "the walk starts at the pid itself"
    );
    assert!(a.len() < 100, "the chain must terminate, not loop: {a:?}");
    assert!(a.contains(&1) || a.len() > 1, "it should reach an ancestor");
}

#[test]
fn a_window_is_matched_only_through_the_client_pid_chain() {
    // This process is its own ancestor, so a client whose pid is ours maps
    // to a window whose pid is ours. A window with an unrelated pid must not.
    let me = std::process::id();
    let windows = vec![client("0xA", me), client("0xB", 999_999)];
    let tmux_clients = vec![ClientPid {
        pid: me,
        session: "dev".to_string(),
    }];

    let map = desktop::map_windows(&windows, &tmux_clients);
    assert_eq!(map, vec![("0xA".to_string(), "dev".to_string())]);
}

#[test]
fn a_window_with_no_tmux_client_inside_is_not_mapped() {
    let windows = vec![client("0xC", 999_999)];
    assert!(desktop::map_windows(&windows, &[]).is_empty());
}

#[test]
fn titles_are_never_used_to_identify_a_session() {
    // A Ghostty window attached to `proj-alpha-2` displays the tmux WINDOW name
    // (`omarchy:proj-discovery`). Matching on the title would bind the
    // wrong session. Proven by giving the title a different session's name.
    let me = std::process::id();
    let mut w = client("0xD", me);
    w.title = "omarchy:some-other-session".to_string();
    let map = desktop::map_windows(
        &[w],
        &[ClientPid {
            pid: me,
            session: "dev".to_string(),
        }],
    );
    assert_eq!(map, vec![("0xD".to_string(), "dev".to_string())]);
}
