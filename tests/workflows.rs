//! What the release workflow is allowed to run.
//!
//! This repository is about to be public, and `release.yml` holds a token that
//! can publish: whatever its steps resolve to at run time is what gets to
//! replace the binary users download and write the checksum that vouches for
//! it. A checksum computed by a compromised step matches its own tampering
//! perfectly, so consistency proves nothing about provenance — only the
//! immutability of what ran does.
//!
//! GitHub's own guidance is that a full commit SHA is the *only* immutable
//! reference for an action: tags and branches move, and `v4` is a tag the
//! action's own maintainers rewrite on every release.
//!
//! These are text checks over the workflow files. There is no YAML parser in
//! this project's dependency tree and adding one to a shipped engine for a
//! test would be the wrong trade; what is checked here is exact enough to
//! fail on any of the shapes a workflow actually uses, and the failure names
//! the file, the line and the value.
//!
//! Not checked here, because it cannot be: whether a pinned SHA is the commit
//! the maintainer read. That is a review, not an assertion. And the repository
//! settings that would *require* pinning across every future workflow are the
//! maintainer's to set — this file only holds the workflows in the tree.

use std::path::PathBuf;

fn workflow_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows")
}

fn workflows() -> Vec<(PathBuf, String)> {
    let mut out: Vec<(PathBuf, String)> = std::fs::read_dir(workflow_dir())
        .expect(".github/workflows exists")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "yml" || e == "yaml"))
        .map(|p| {
            let text = std::fs::read_to_string(&p).unwrap();
            (p, text)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(
        !out.is_empty(),
        "no workflows found — this check is not looking at anything"
    );
    out
}

/// The value of a `key:` line, with any trailing `# comment` and quotes off.
fn value_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let t = line.trim_start();
    let t = t.strip_prefix("- ").unwrap_or(t);
    let rest = t.strip_prefix(key)?.strip_prefix(':')?;
    let rest = match rest.split_once(" #") {
        Some((before, _)) => before,
        None => rest,
    };
    Some(rest.trim().trim_matches('"').trim_matches('\''))
}

fn is_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Every `uses:` names a full commit SHA.
///
/// `actions/checkout@v4` is a tag on a repository this project does not
/// control. Whoever can move it — the maintainers, or anyone who takes their
/// account — chooses what runs in the publish job, which has `contents:
/// write`. A 40-character commit SHA cannot be moved onto different content.
#[test]
fn every_action_is_pinned_to_a_full_commit_sha() {
    let mut bad = Vec::new();
    for (path, text) in workflows() {
        for (n, line) in text.lines().enumerate() {
            let Some(value) = value_of(line, "uses") else {
                continue;
            };
            // A local action is this repository's own content, already fixed
            // by the commit the workflow runs from.
            if value.starts_with("./") {
                continue;
            }
            let pinned = value.rsplit_once('@').is_some_and(|(_, r)| is_sha(r));
            if !pinned {
                bad.push(format!(
                    "{}:{}: uses: {value} — a tag or branch can be moved onto \
                     other code; pin the 40-character commit SHA",
                    path.display(),
                    n + 1
                ));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "mutable action references:\n{}",
        bad.join("\n")
    );
}

/// Every job container names an image digest.
///
/// `rust:1-bookworm` is republished; the release binary is built inside it and
/// the checksum is computed there. An image that changed between the review
/// and the run is a build nobody reviewed.
///
/// A `${{ … }}` value is an indirection to another `image:` line in the same
/// file, which this same check holds to the same rule.
#[test]
fn every_container_image_is_pinned_to_a_digest() {
    let mut bad = Vec::new();
    for (path, text) in workflows() {
        for (n, line) in text.lines().enumerate() {
            let Some(value) = value_of(line, "image") else {
                continue;
            };
            if value.starts_with("${{") {
                continue;
            }
            let pinned = value.split_once("@sha256:").is_some_and(|(_, d)| {
                d.len() == 64
                    && d.bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            });
            if !pinned {
                bad.push(format!(
                    "{}:{}: image: {value} — a tag is republished; pin @sha256:<digest>",
                    path.display(),
                    n + 1
                ));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "mutable container images:\n{}",
        bad.join("\n")
    );
}

/// The job that can publish checks out nothing.
///
/// It needs the built artifact and a token, and it has both. A checkout is
/// another action running inside the one job that holds `contents: write`,
/// for source code no step in that job reads — the release is created with
/// `gh release create --repo "$GITHUB_REPOSITORY"`, which asks GitHub, not
/// the working tree.
#[test]
fn the_publishing_job_checks_out_nothing() {
    let path = workflow_dir().join("release.yml");
    let text = std::fs::read_to_string(&path).expect("release.yml exists");
    let body = job_body(&text, "publish").expect("release.yml has a publish job");
    assert!(
        !body.contains("actions/checkout"),
        "the publish job holds contents: write and runs a checkout it does not \
         need:\n{body}"
    );
    // The check is only worth having if it is looking at the right job.
    assert!(
        body.contains("contents: write"),
        "publish job body does not look like the publishing job:\n{body}"
    );
}

/// One job's block, from its two-space-indented key to the next one.
fn job_body(text: &str, job: &str) -> Option<String> {
    let mut lines = text.lines();
    let mut body = String::new();
    let head = format!("  {job}:");
    lines.by_ref().position(|l| l.trim_end() == head)?;
    for line in lines {
        let indented = line.starts_with("    ") || line.trim().is_empty();
        if !indented {
            break;
        }
        body.push_str(line);
        body.push('\n');
    }
    Some(body)
}

/// The workflows this check reads are the ones the repository actually has.
///
/// A rename would otherwise leave every assertion above scanning nothing and
/// passing.
#[test]
fn the_workflows_this_file_checks_are_present() {
    let names: Vec<String> = workflows()
        .iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    for expected in ["ci.yml", "release.yml"] {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} is missing; found {names:?}"
        );
    }
}
