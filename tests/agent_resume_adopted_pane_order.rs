//! An **adopted** session's panes must be paired with the captured ones the
//! way adoption itself validated them — by layout cell — and never by the
//! order tmux happened to create them in.
//!
//! # The defect this test exists for
//!
//! Adoption proves a live session is the captured one by walking both sides
//! in *layout-cell* order and comparing each cell's working directory (see
//! `equiv`). The resume pass then paired the same panes by **pane-id creation
//! order**: it sorted the window's live panes by ascending `%N` and matched
//! them positionally against the captured panes.
//!
//! Those two orders are the same only for a window whose panes were never
//! moved. A window built one way and rearranged — `swap-pane`, a pane closed
//! and reopened, any of the ordinary things a person does to a layout — has
//! them disagree, and the conversation is then delivered into the wrong pane
//! of the right window. Nothing about the restore looks wrong afterwards:
//! `agents_resumed` is 1 and the state is `succeeded`. Only *which* pane is
//! running it is wrong.
//!
//! So this test builds exactly that: the destination's `dev` matches the
//! snapshot cell for cell, and its panes were created in the opposite order.
//! The assertion is which working directory the agent came back in.
//!
//! # One test per file
//!
//! `capture` and `restore` run in-process and read `$OSM_CLAUDE_HOME` and
//! `$XDG_CONFIG_HOME`, and the panes need the stub agent on `$PATH`.

mod common;

use osm::tmux::Tmux;
use std::time::Duration;

struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        common::shutdown(&self.0);
    }
}

fn sock(label: &str) -> String {
    format!("osm-adoptorder-{}-{}", label, std::process::id())
}

const ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844b05";

fn cmd_at(panes: &[osm::tmux::PaneRec], cwd: &std::path::Path) -> String {
    let want = cwd.to_str().unwrap();
    panes
        .iter()
        .find(|p| p.cwd == want)
        .unwrap_or_else(|| panic!("no pane at {want}: {panes:?}"))
        .cmd
        .clone()
}

fn pane_id_at(panes: &[osm::tmux::PaneRec], cwd: &std::path::Path) -> String {
    let want = cwd.to_str().unwrap();
    panes
        .iter()
        .find(|p| p.cwd == want)
        .unwrap_or_else(|| panic!("no pane at {want}: {panes:?}"))
        .id
        .clone()
}

#[test]
fn an_adopted_window_resumes_into_the_pane_the_layout_says_not_the_oldest() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::claude_fixture(tmp.path(), ID);
    let bin = common::stub_agent(tmp.path());

    let config_home = tmp.path().join("config");
    std::fs::create_dir_all(config_home.join("osm")).unwrap();
    std::fs::write(
        config_home.join("osm/config.toml"),
        "[agents]\nenabled = [\"claude\"]\nauto_resume = true\nauto_resume_max_age_mins = 30\n",
    )
    .unwrap();
    std::env::set_var(
        "PATH",
        format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    std::env::set_var("OSM_CLAUDE_HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", &config_home);
    common::tmux_conf_with_path(&config_home, &bin);

    // `top` is the window's first layout cell, `bottom` the second. The agent
    // runs in `bottom`, so a pairing that is off by one lands in `top` — a
    // pane that exists and is an idle shell, which is what makes the wrong
    // answer look like a right one.
    let top = tmp.path().join("top");
    let bottom = tmp.path().join("bottom");
    for d in [&top, &bottom] {
        std::fs::create_dir_all(d).unwrap();
    }

    // ---- before the reboot: panes created in layout order -------------------
    let src = Server(Tmux::with_socket(&sock("src")));
    src.0
        .run(&[
            "new-session",
            "-d",
            "-s",
            "dev",
            "-n",
            "code",
            "-c",
            top.to_str().unwrap(),
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();
    src.0
        .run(&[
            "split-window",
            "-t",
            "=dev:code",
            "-c",
            bottom.to_str().unwrap(),
        ])
        .unwrap();
    let src_panes = src.0.list_panes().unwrap();
    let src_agent = pane_id_at(&src_panes, &bottom);
    src.0
        .run(&[
            "send-keys",
            "-t",
            &src_agent,
            &format!("claude --resume {ID}"),
            "C-m",
        ])
        .unwrap();
    assert_eq!(
        common::wait_for_pane_cmd(&src.0, &src_agent, "claude", Duration::from_secs(10)),
        "claude",
        "the stub agent never started in the source pane"
    );

    let mut conn = osm::db::open(&tmp.path().join("state.db")).unwrap();
    let snap = osm::capture::snapshot(&mut conn, &src.0, "before-reboot").unwrap();
    conn.execute(
        "UPDATE snapshots SET boot_id='boot-previous' WHERE id=?1",
        [snap],
    )
    .unwrap();
    drop(src);

    let adapter = osm::agent::claude::Claude::with_home(&home);
    let gone_by = std::time::Instant::now() + Duration::from_secs(10);
    while osm::agent::detect::live_process_ownership(&adapter, ID).unwrap()
        != osm::agent::Liveness::Inactive
        && std::time::Instant::now() < gone_by
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        osm::agent::detect::live_process_ownership(&adapter, ID).unwrap(),
        osm::agent::Liveness::Inactive
    );

    // ---- after the reboot: the same session, built backwards ---------------
    //
    // `bottom` is created first and therefore gets the lower `%N`; `top` is
    // split off it and then swapped into the first cell. The session is now
    // cell-for-cell what the snapshot holds — so restore adopts it — while its
    // pane ids run the other way.
    let dst = Server(Tmux::with_socket(&sock("dst")));
    dst.0
        .run(&[
            "new-session",
            "-d",
            "-s",
            "dev",
            "-n",
            "code",
            "-c",
            bottom.to_str().unwrap(),
            "-x",
            "200",
            "-y",
            "50",
        ])
        .unwrap();
    dst.0
        .run(&[
            "split-window",
            "-t",
            "=dev:code",
            "-c",
            top.to_str().unwrap(),
        ])
        .unwrap();
    let before = dst.0.list_panes().unwrap();
    let dst_bottom = pane_id_at(&before, &bottom);
    let dst_top = pane_id_at(&before, &top);
    dst.0
        .run(&["swap-pane", "-d", "-s", &dst_bottom, "-t", &dst_top])
        .unwrap();
    // The captured session's active pane is the one the split created, which
    // is the *second* layout cell; the adoption compares that too.
    dst.0.run(&["select-pane", "-t", &dst_bottom]).unwrap();

    assert!(
        dst_bottom.trim_start_matches('%').parse::<u64>().unwrap()
            < dst_top.trim_start_matches('%').parse::<u64>().unwrap(),
        "the agent's pane must be the *older* of the two, so creation order \
         and layout order disagree: bottom={dst_bottom} top={dst_top}"
    );
    for p in &before {
        assert_eq!(
            common::wait_for_pane_cmd(&dst.0, &p.id, "bash", Duration::from_secs(10)),
            "bash"
        );
    }

    let report = osm::restore::run_restore(&mut conn, &dst.0, false).unwrap();
    let json = osm::ipc::RestoreJson::from_report(&report);
    assert_eq!(
        json.adopted,
        vec!["dev".to_string()],
        "the live session must be adopted, or this test exercises nothing: {json:?}"
    );
    assert_eq!(report.state, "succeeded", "reason={}", report.reason);
    assert_eq!(json.agents_resumed, 1, "{json:?}");

    let after = dst.0.list_panes().unwrap();
    assert_eq!(
        cmd_at(&after, &bottom),
        "claude",
        "the conversation belongs in the pane whose layout cell it was \
         captured in: {after:?}"
    );
    assert_eq!(
        cmd_at(&after, &top),
        "bash",
        "the other pane held no conversation and must still be a shell: {after:?}"
    );
}
