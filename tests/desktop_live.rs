//! Checks that only a real compositor can answer.
//!
//! These exist because Hyprland's dispatch syntax cannot be verified by
//! reasoning. `hyprctl dispatch exec "[workspace 4 silent] …"` reads as
//! obviously correct and fails on 0.56 with `']' expected near '4'`; the
//! Lua form works. A fixture cannot tell you that.
//!
//! **Two gates, both required**, because these run on a developer's live
//! desktop:
//!   * `HYPRLAND_INSTANCE_SIGNATURE` — there is a compositor at all
//!   * `OSM_LIVE_DESKTOP_TESTS=1` — a human has opted in, right now
//!
//! Read-only checks need only the first. Anything that could put a window on
//! screen needs both, and even then cleans up after itself. A test once
//! spawned a terminal onto the maintainer's desktop; the opt-in is what makes
//! that a decision rather than an accident.

use osm::hypr::{self, HyprCtl};

fn compositor() -> bool {
    std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok()
}

fn opted_in() -> bool {
    std::env::var("OSM_LIVE_DESKTOP_TESTS").as_deref() == Ok("1")
}

#[test]
fn the_lua_dispatch_shape_is_accepted_by_this_compositor() {
    if !compositor() {
        eprintln!("skipped: no HYPRLAND_INSTANCE_SIGNATURE");
        return;
    }
    // Names a window that cannot exist. A compositor that understands the
    // shape rejects it for naming a missing window; one that does not
    // rejects it as malformed. That difference is the whole test, and it
    // moves nothing either way.
    let h = hypr::Live::new();
    let text = match h
        .dispatch("hl.dsp.window.move({window='address:0xdeadbeef', workspace='1', follow=false})")
    {
        Ok(s) => s,
        Err(e) => e.to_string(),
    };
    let lower = text.to_lowercase();
    assert!(
        !lower.contains("expected") && !lower.contains("syntax") && !lower.contains("lua"),
        "this compositor rejected the dispatch as malformed, so place_lua is wrong for it: {text}"
    );
}

#[test]
fn clients_and_monitors_parse_against_this_compositor() {
    if !compositor() {
        eprintln!("skipped: no HYPRLAND_INSTANCE_SIGNATURE");
        return;
    }
    let h = hypr::Live::new();
    let ms = hypr::parse_monitors(&h.monitors_json().unwrap()).unwrap();
    let cs = hypr::parse_clients(&h.clients_json().unwrap()).unwrap();
    assert!(!ms.is_empty(), "a live compositor has at least one monitor");

    // Every window must resolve to a monitor that exists. A client reports
    // its monitor as an index and nothing else; if that lookup fails here,
    // capture would store a connector nothing matches and placement would
    // silently drop every window onto the focused output.
    for c in &cs {
        assert!(
            hypr::connector_of(c.monitor_index, &ms).is_some(),
            "window {} claims monitor index {} which no monitor has: {:?}",
            c.address,
            c.monitor_index,
            ms.iter().map(|m| (m.id, &m.name)).collect::<Vec<_>>()
        );
    }
}

#[test]
fn a_spawned_terminal_is_found_by_lineage_and_placed() {
    if !compositor() || !opted_in() {
        eprintln!("skipped: needs a compositor and OSM_LIVE_DESKTOP_TESTS=1");
        return;
    }
    // The one test that puts a window on screen. It uses its own private
    // tmux server, and closes what it opened.
    let t = osm::tmux::Tmux::with_socket(&format!("osm-live-{}", std::process::id()));
    t.run(&["new-session", "-d", "-s", "osmlive", "-c", "/tmp"])
        .expect("private tmux server");

    let h = hypr::Live::new();
    let before: Vec<String> = hypr::parse_clients(&h.clients_json().unwrap())
        .unwrap()
        .into_iter()
        .map(|c| c.address)
        .collect();

    let p = osm::desktop::Placement {
        session: "osmlive".into(),
        address: String::new(),
        class: String::new(),
        terminal_kind: "auto".into(),
        workspace_kind: "numbered".into(),
        workspace_ref: std::env::var("OSM_LIVE_WORKSPACE").unwrap_or_else(|_| "9".into()),
        monitor_connector: String::new(),
        monitor_desc: None,
        monitor_scale: None,
        monitor_transform: None,
        floating: false,
        rel: None,
    };

    // Record the process we start, so cleanup can prove what belongs to us.
    struct Watching {
        inner: osm::desktop::RealSpawner,
        seen: std::cell::Cell<Option<osm::desktop::Spawned>>,
    }
    impl osm::desktop::Spawner for Watching {
        fn spawn(&self, argv: &[String]) -> anyhow::Result<osm::desktop::Spawned> {
            let s = self.inner.spawn(argv)?;
            self.seen.set(Some(s));
            Ok(s)
        }
        fn kill(&self, s: &osm::desktop::Spawned) {
            self.inner.kill(s);
        }
    }
    let watcher = Watching {
        inner: osm::desktop::RealSpawner::new(),
        seen: std::cell::Cell::new(None),
    };

    let outcome = osm::desktop::spawn_and_place(
        &h,
        &watcher,
        &t,
        &p,
        "osm-live-test",
        "auto",
        std::time::Duration::from_secs(15),
    );

    // Close only what this test owns.
    //
    // "Anything that appeared since we started" is not ownership: the
    // developer may open a browser while this runs, and closing it would be
    // this project doing to them exactly what it exists to prevent. A window
    // is ours only if our own spawned process is in its ancestry.
    let spawned = watcher.seen.get();
    let after = hypr::parse_clients(&h.clients_json().unwrap()).unwrap();
    for c in after
        .iter()
        .filter(|c| !before.contains(&c.address))
        .filter(|c| spawned.is_some_and(|s| osm::desktop::owns_window(&s, c.pid)))
    {
        let _ = h.dispatch(&format!(
            "hl.dsp.window.close({{window='address:{}'}})",
            c.address
        ));
    }
    let _ = t.run(&["kill-server"]);
    let _ = spawned;

    assert!(
        matches!(outcome, osm::desktop::PlaceOutcome::Placed(_)),
        "a terminal this test started was not found and placed: {outcome:?}"
    );
}
