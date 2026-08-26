use osm::agent::resume::{self, Outcome};
use osm::tmux::Tmux;

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.run(&["kill-server"]);
        if let Some(s) = self.0.socket() {
            let _ = std::fs::remove_file(format!("/tmp/tmux-{}/{s}", real_uid()));
        }
    }
}

// Avoid a libc dependency: the real uid is readable straight out of
// /proc/self/status, which every test target already relies on elsewhere
// in this project (detect.rs walks /proc for the same reason).
fn real_uid() -> u32 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1000)
}

fn server(label: &str) -> Server {
    let t = Tmux::with_socket(&format!("osm-pre-{label}-{}", std::process::id()));
    t.run(&["new-session", "-d", "-s", "alpha", "-c", "/tmp"])
        .unwrap();
    Server(t)
}

#[test]
fn a_pane_that_is_not_in_the_live_set_is_missing_not_resolved() {
    // tmux `-t` matching is fuzzy: a missing pane index resolves to another
    // pane, so target resolution must never be used as an existence test.
    let s = server("missing");
    let a = osm::agent::claude::Claude::new();
    let live: Vec<String> =
        s.0.list_panes()
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();

    let out = resume::preflight(&s.0, "%999", &a, "abc", &live);
    assert!(matches!(out, Outcome::PaneMissing), "{out:?}");
}

#[test]
fn a_pane_running_something_other_than_a_shell_is_busy() {
    let s = server("busy");
    // `=alpha:` (trailing colon) is the exact session-then-active-pane
    // form; `=alpha` alone is parsed by send-keys as an exact *pane* name
    // and never matches a session, which fails outright on tmux 3.7c.
    s.0.run(&["send-keys", "-t", "=alpha:", "sleep 30", "C-m"])
        .unwrap();
    // wait for the foreground command to become `sleep`
    let pane = s.0.list_panes().unwrap()[0].id.clone();
    for _ in 0..100 {
        if s.0.list_panes().unwrap()[0].cmd == "sleep" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        s.0.list_panes().unwrap()[0].cmd,
        "sleep",
        "fixture never started"
    );

    let a = osm::agent::claude::Claude::new();
    let live = vec![pane.clone()];
    let out = resume::preflight(&s.0, &pane, &a, "abc", &live);
    assert!(matches!(out, Outcome::PaneBusy), "{out:?}");
}

#[test]
fn an_idle_shell_with_no_conflict_passes_preflight() {
    let s = server("idle");
    let pane = s.0.list_panes().unwrap()[0].id.clone();
    // A freshly forked pane briefly reports its foreground command as
    // `tmux` itself (the fork's pre-exec image) before it settles to the
    // shell; wait for that to resolve, same as the busy test above does
    // for its own foreground command.
    for _ in 0..100 {
        if s.0.list_panes().unwrap()[0].cmd == "bash" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        s.0.list_panes().unwrap()[0].cmd,
        "bash",
        "fixture never settled"
    );

    let a = osm::agent::claude::Claude::with_home(std::path::Path::new("/nonexistent"));
    let live = vec![pane.clone()];
    let out = resume::preflight(&s.0, &pane, &a, "abc", &live);
    assert!(matches!(out, Outcome::Resumed), "{out:?}");
}

#[test]
fn outcomes_have_stable_machine_readable_names() {
    assert_eq!(Outcome::Resumed.as_str(), "resumed");
    assert_eq!(Outcome::ActiveElsewhere.as_str(), "active_elsewhere");
    assert_eq!(Outcome::PaneBusy.as_str(), "pane_busy");
    assert_eq!(Outcome::PaneMissing.as_str(), "pane_missing");
    assert_eq!(Outcome::Unsupported.as_str(), "unsupported");
    assert_eq!(Outcome::OwnershipUnknown.as_str(), "ownership_unknown");
    assert_eq!(Outcome::Failed("x".into()).as_str(), "failed");
}
