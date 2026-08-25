//! A test that starts a tmux server has to clean up after itself completely.
//!
//! `kill-server` stops the server but leaves its socket file behind, and every
//! suite here names its socket uniquely per run, so each `cargo test` used to
//! deposit another ~80 dead files in the tmux socket directory — some three
//! thousand of them by the time anyone noticed. They are inert, but they
//! accumulate forever and they sit in a directory shared with the developer's
//! real tmux server, which is the last place anyone should be tempted to run a
//! broad `rm`. The fix is to not create them, not to sweep them.

mod common;

use osm::tmux::Tmux;

#[test]
fn the_shutdown_helper_removes_the_socket_file_it_created() {
    let socket = format!("osm-sockclean-{}", std::process::id());
    let tmux = Tmux::with_socket(&socket);
    tmux.run(&["new-session", "-d", "-s", "probe", "-c", "/tmp"])
        .unwrap();

    let created: Vec<_> = common::socket_paths(&tmux)
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    assert_eq!(
        created.len(),
        1,
        "the server must have created exactly one socket file for this test, \
         otherwise this test is asserting nothing: {created:?}"
    );

    common::shutdown(&tmux);

    let left: Vec<_> = common::socket_paths(&tmux)
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    assert!(
        left.is_empty(),
        "the helper must leave no socket file behind: {left:?}"
    );
}
