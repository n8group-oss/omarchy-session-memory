//! `osm install` and `osm uninstall`, and the promise that both can be
//! inspected before they act.
//!
//! `omarchy plugin add` copies QML and nothing else. Someone has to place the
//! binary and the two systemd units — and, more importantly, someone has to
//! remove them. An uninstall that leaves a daemon running and hooks firing at
//! a binary that is no longer there is worse than no uninstall at all.
//!
//! Every test here works against a temporary prefix. Nothing in
//! `osm::install` starts a process, so nothing here can reach the developer's
//! systemd, their tmux server, or their snapshot database.

use std::fs;

#[test]
fn a_dry_run_changes_nothing_and_says_what_it_would_do() {
    let tmp = tempfile::tempdir().unwrap();
    let prefix = tmp.path();
    let plan = osm::install::plan(prefix).unwrap();

    let lines = osm::install::install(&plan, true).unwrap();
    assert!(!lines.is_empty(), "a dry run still reports its plan");
    assert!(
        lines.iter().any(|l| l.contains("osm")),
        "it names the binary: {lines:?}"
    );
    assert!(
        !prefix.join("bin/osm").exists(),
        "a dry run must not install anything"
    );
}

#[test]
fn uninstall_removes_units_and_hooks_and_asks_about_the_database() {
    let tmp = tempfile::tempdir().unwrap();
    let prefix = tmp.path();
    fs::create_dir_all(prefix.join("bin")).unwrap();
    fs::write(prefix.join("bin/osm"), b"#!/bin/sh\n").unwrap();

    let lines = osm::install::uninstall(&osm::install::plan(prefix).unwrap(), true, true).unwrap();
    assert!(
        lines.iter().any(|l| l.to_lowercase().contains("database")),
        "uninstall must say what happens to the user's snapshots: {lines:?}"
    );
    assert!(prefix.join("bin/osm").exists(), "dry run removed a file");
}

#[test]
fn uninstall_keeps_the_database_unless_told_otherwise() {
    // Snapshots are the user's data. Removing the tool must not remove the
    // record of where they were working, unless they say so.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state/osm/state.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    fs::write(&db, b"x").unwrap();

    let lock = db.parent().unwrap().join("restore.lock");
    osm::install::uninstall_database(&db, &lock, false).unwrap();
    assert!(db.exists(), "the database survives a default uninstall");

    osm::install::uninstall_database(&db, &lock, true).unwrap();
    assert!(!db.exists(), "and is removed only when asked");
}

#[test]
fn a_real_install_writes_the_binary_and_both_units() {
    let tmp = tempfile::tempdir().unwrap();
    let prefix = tmp.path();
    let plan = osm::install::plan(prefix).unwrap();
    assert_eq!(plan.units.len(), 2, "{:?}", plan.units);

    let lines = osm::install::install(&plan, false).unwrap();
    assert!(plan.binary.exists(), "{lines:?}");
    for unit in &plan.units {
        assert!(
            unit.exists(),
            "{} was not written: {lines:?}",
            unit.display()
        );
    }
}

/// A unit whose `ExecStart` names a path the install did not write starts
/// nothing, and systemd reports it as a unit failure with no hint that the
/// binary is somewhere else entirely.
#[test]
fn the_units_point_at_the_binary_this_install_placed() {
    let tmp = tempfile::tempdir().unwrap();
    let plan = osm::install::plan(tmp.path()).unwrap();
    osm::install::install(&plan, false).unwrap();

    let want = plan.binary.display().to_string();
    for unit in &plan.units {
        let text = fs::read_to_string(unit).unwrap();
        assert!(
            text.contains(&want),
            "{} does not name {want}:\n{text}",
            unit.display()
        );
        assert!(
            !text.contains("%h/"),
            "{} still carries an unresolved placeholder:\n{text}",
            unit.display()
        );
    }
}

/// The whole point of an uninstall: what the install placed is gone
/// afterwards.
#[test]
fn uninstall_removes_exactly_what_install_wrote() {
    let tmp = tempfile::tempdir().unwrap();
    let prefix = tmp.path();
    let plan = osm::install::plan(prefix).unwrap();
    osm::install::install(&plan, false).unwrap();

    let lines = osm::install::uninstall(&plan, true, false).unwrap();
    assert!(!plan.binary.exists(), "the binary survived: {lines:?}");
    for unit in &plan.units {
        assert!(!unit.exists(), "{} survived: {lines:?}", unit.display());
    }
}

/// Uninstalling something that was never installed is not an error, and must
/// not claim to have removed anything.
#[test]
fn uninstall_of_an_empty_prefix_removes_nothing_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let lines =
        osm::install::uninstall(&osm::install::plan(tmp.path()).unwrap(), true, false).unwrap();
    assert!(
        !lines.iter().any(|l| l.starts_with("removed ")),
        "nothing was there to remove: {lines:?}"
    );
}

// ---------------------------------------------------------------------------
// Prefix resolution, against the kernel rather than against a rule of thumb.
//
// `normalize` folded `..` lexically before resolving symlinks, which is not
// what a path means. On this developer's machine `/bin` is a symlink to
// `usr/bin`, so `/bin/../tmp` is `/usr/tmp` to the kernel and `/tmp` to the
// folder — and `/tmp` is a real directory full of other people's files. A
// prefix a user typed could therefore install into, and later *remove* from,
// a directory they did not name.
//
// So these tests build the symlink themselves rather than relying on this
// machine's `/bin`, and check the answer against `std::fs::canonicalize`,
// which is the kernel's own resolver.
// ---------------------------------------------------------------------------

/// `root/bin` → `root/opt/pkg/bin`, so `root/bin/..` is `root/opt/pkg`.
fn symlinked_tree(root: &std::path::Path) {
    fs::create_dir_all(root.join("opt/pkg/bin")).unwrap();
    fs::create_dir_all(root.join("opt/pkg/lib")).unwrap();
    // The lexical answer, which must *not* be what comes back. It exists so
    // the two candidates are both real directories and the test cannot pass
    // by accident of one of them being absent.
    fs::create_dir_all(root.join("lib")).unwrap();
    std::os::unix::fs::symlink("opt/pkg/bin", root.join("bin")).unwrap();
}

#[test]
fn a_parent_step_out_of_a_symlink_lands_where_the_kernel_lands() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    symlinked_tree(&root);

    let scenic = root.join("bin/../lib");
    let got = osm::install::normalize(&scenic).unwrap();

    assert_eq!(
        got,
        std::fs::canonicalize(&scenic).unwrap(),
        "normalize disagrees with the kernel about {}",
        scenic.display()
    );
    assert_eq!(
        got,
        root.join("opt/pkg/lib"),
        "`bin/../lib` steps out of the *link target*, not out of the directory \
         the link sits in"
    );
    assert_ne!(
        got,
        root.join("lib"),
        "the lexical answer manages an unrelated directory"
    );
}

/// The half that made this hard: a prefix that does not exist yet is the
/// ordinary case the first time, and `canonicalize` refuses it outright. The
/// part that *does* exist still has to resolve through its symlinks.
#[test]
fn a_prefix_that_does_not_exist_yet_still_resolves_the_part_that_does() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    symlinked_tree(&root);

    let scenic = root.join("bin/../lib/not-yet/deeper");
    assert!(
        std::fs::canonicalize(&scenic).is_err(),
        "the tail must be absent"
    );

    assert_eq!(
        osm::install::normalize(&scenic).unwrap(),
        root.join("opt/pkg/lib/not-yet/deeper"),
    );
}

/// The case the lexical fold was written for, which must keep working:
/// `<prefix>/.local/../.local` is `<prefix>/.local`, and the install decides
/// whether to touch systemd by comparing against exactly that.
#[test]
fn a_parent_step_out_of_a_real_directory_still_folds() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(root.join(".local")).unwrap();

    assert_eq!(
        osm::install::normalize(&root.join(".local/../.local")).unwrap(),
        root.join(".local"),
    );
}

/// A `..` at the root has nowhere to go, exactly as it has for the kernel.
#[test]
fn a_parent_step_at_the_root_stays_at_the_root() {
    assert_eq!(
        osm::install::normalize(std::path::Path::new("/../..")).unwrap(),
        std::path::PathBuf::from("/"),
    );
}

/// A symlink pointing at itself must produce an answer or an error, never a
/// hang: `normalize` runs before anything is written, and a wedged `osm
/// install` is a machine with no engine and no message.
#[test]
fn a_symlink_loop_is_refused_rather_than_followed_forever() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::os::unix::fs::symlink(root.join("b"), root.join("a")).unwrap();
    std::os::unix::fs::symlink(root.join("a"), root.join("b")).unwrap();

    let err = osm::install::normalize(&root.join("a/prefix"))
        .expect_err("a symlink loop must not resolve");
    let text = format!("{err:#}");
    assert!(
        text.contains("symlink") || text.contains("link"),
        "the error must say what went wrong: {text}"
    );
}

/// An absolute symlink target starts again at the root, rather than being
/// appended to wherever the link happened to live.
#[test]
fn an_absolute_symlink_target_restarts_at_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("elsewhere/lib")).unwrap();
    std::os::unix::fs::symlink(root.join("elsewhere"), root.join("here")).unwrap();

    let scenic = root.join("here/lib");
    assert_eq!(
        osm::install::normalize(&scenic).unwrap(),
        std::fs::canonicalize(&scenic).unwrap(),
    );
    assert_eq!(
        osm::install::normalize(&scenic).unwrap(),
        root.join("elsewhere/lib"),
    );
}

/// A component that exists and is **not a directory** cannot be traversed.
///
/// `normalize` asked only "is this a symlink?", and treated every other
/// answer — a regular file, a socket, a device, or a `symlink_metadata` that
/// failed outright — as a component to append and walk on from. So
/// `/etc/passwd/../../tmp/osm-prefix-proof`, which the kernel refuses with
/// `ENOTDIR`, folded neatly to `/tmp/osm-prefix-proof`: `osm install`
/// proposed installing into a directory the user had not named, and a later
/// `osm uninstall` given the same argument would remove files from it.
#[test]
fn a_path_through_a_regular_file_is_refused_the_way_the_kernel_refuses_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::write(root.join("passwd"), b"not a directory\n").unwrap();
    // The destination the lexical fold would have landed on, made real so the
    // test cannot pass merely because it is absent.
    fs::create_dir_all(root.join("tmp")).unwrap();

    let scenic = root.join("passwd/../tmp/prefix");
    assert!(
        std::fs::canonicalize(&scenic).is_err(),
        "the kernel must refuse this path, or the test is not about anything"
    );

    let err = osm::install::normalize(&scenic)
        .expect_err("a path through a regular file must not resolve");
    let text = format!("{err:#}");
    assert!(
        text.contains("passwd"),
        "the refusal must name the component that is not a directory: {text}"
    );
    assert!(
        text.contains(&scenic.display().to_string()),
        "and it must name the prefix the user actually typed, not the one it \
         folded to: {text}"
    );
}

/// The same hole, at the exact path the review named.
///
/// `/etc/passwd` is a regular file on every machine this runs on, and
/// `normalize` is a pure function that reads metadata and nothing else — no
/// file under `/etc` or `/tmp` is opened, created or removed by this test.
#[test]
fn the_reviewers_prefix_does_not_normalise_to_somewhere_else_entirely() {
    let scenic = std::path::Path::new("/etc/passwd/../../tmp/osm-prefix-proof");
    let got = osm::install::normalize(scenic);
    assert!(
        got.is_err(),
        "`osm install --prefix {}` was normalised to {:?}, a directory the \
         user never named and an uninstall would later delete from",
        scenic.display(),
        got.ok()
    );
}

/// A `symlink_metadata` that fails for a reason other than absence is not
/// evidence that the component is absent.
///
/// It was swallowed by `unwrap_or(false)` — "not a symlink" — and the
/// component was appended as though the path simply had not been created yet.
/// A name too long for the filesystem is the reproducible case: the kernel
/// answers `ENAMETOOLONG`, which says nothing whatsoever about what is there.
#[test]
fn a_metadata_error_that_is_not_absence_is_propagated() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let too_long = "n".repeat(300);
    let path = root.join(&too_long).join("prefix");

    let err = std::fs::symlink_metadata(root.join(&too_long))
        .expect_err("the fixture must be a name this filesystem refuses");
    assert_ne!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "the fixture must fail for a reason other than absence: {err}"
    );

    osm::install::normalize(&path)
        .expect_err("a component osm could not read must not be assumed absent");
}

/// A `..` after a component that does not exist has nowhere to step out of.
///
/// The kernel answers `ENOENT`; the folder stepped out of the missing name
/// lexically and carried on, so a prefix naming a directory that is not there
/// resolved to a completely different one that is. Appending a nonexistent
/// tail is still allowed — that is the ordinary first install — but only when
/// nothing after it walks back out.
#[test]
fn a_parent_step_out_of_a_component_that_does_not_exist_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("real")).unwrap();

    let scenic = root.join("not-yet/../real");
    assert!(
        std::fs::canonicalize(&scenic).is_err(),
        "the kernel must refuse this path, or the test is not about anything"
    );
    let got = osm::install::normalize(&scenic);
    assert!(
        got.is_err(),
        "a prefix that steps back out of a directory that is not there \
         resolved to {got:?}"
    );

    // The control: a tail that does not exist and never steps back out is the
    // ordinary first-install case and must still resolve.
    assert_eq!(
        osm::install::normalize(&root.join("real/not-yet/deeper")).unwrap(),
        root.join("real/not-yet/deeper"),
    );
}
