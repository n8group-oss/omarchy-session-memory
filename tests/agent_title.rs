//! What a conversation is *about*, in one short line.
//!
//! # Why this file exists
//!
//! The maintainer opened the menu on their own machine and could not tell one
//! session from another: *"for me there is no special information what is in
//! particular session. we should have option to go deep and try to grab the
//! session goal."* A row that reads `4 panes · 4 agents · workspace 4` is true
//! of most of their eight sessions at once.
//!
//! # The rule these tests hold to
//!
//! A title is either something the agent wrote about itself, or one line of
//! the first thing the user typed — never a paraphrase, never message bodies,
//! and never anything from a conversation osm cannot attribute. Where neither
//! exists the answer is `None`, which the menu renders as *untitled*: a
//! fabricated title is worse than no title, and a blank one reads as "this
//! session has no purpose".
//!
//! # Bounded, because the store is not
//!
//! The maintainer's Codex store is 14 GB across 2383 rollouts and their Claude
//! store 1.4 GB. Every extraction here reads a bounded prefix or suffix of one
//! file and never the file itself, which is what the two "past the bound"
//! tests below pin: a marker beyond the bound is *not* found, and that is the
//! correct answer rather than a defect.

use osm::agent::title::{Policy, Source, Title, MAX_CHARS};
use osm::agent::{AgentAdapter, AgentSession};
use std::fs;
use std::path::Path;

fn write(path: &Path, bytes: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

const CLAUDE_ID: &str = "0cfebf91-81c0-43d5-af63-c9fe7e844ede";
const CODEX_ID: &str = "b70babcd-65cf-4760-b99b-e8fe1d07d290";

/// A Claude home holding one transcript with `body`, and that transcript's
/// session as `discover` reports it.
fn claude(root: &Path, body: &str) -> (osm::agent::claude::Claude, AgentSession) {
    write(
        &root
            .join("projects/-home-u-app")
            .join(format!("{CLAUDE_ID}.jsonl")),
        body,
    );
    let adapter = osm::agent::claude::Claude::with_home(root);
    let session = adapter
        .discover()
        .unwrap()
        .into_iter()
        .next()
        .expect("the fixture transcript was discovered");
    (adapter, session)
}

/// The same for Codex, whose conversations are `rollout-<stamp>-<id>.jsonl`.
fn codex(root: &Path, body: &str) -> (osm::agent::codex::Codex, AgentSession) {
    write(
        &root
            .join("sessions/2026/08/24")
            .join(format!("rollout-2026-08-24T10-00-00-{CODEX_ID}.jsonl")),
        body,
    );
    let adapter = osm::agent::codex::Codex::with_home(root);
    let session = adapter
        .discover()
        .unwrap()
        .into_iter()
        .next()
        .expect("the fixture rollout was discovered");
    (adapter, session)
}

/// Claude writes its own title into the transcript and revises it as the
/// conversation goes on. The last revision is the one that describes what the
/// conversation became.
#[test]
fn a_claude_conversation_is_named_by_the_last_title_the_agent_wrote() {
    let tmp = tempfile::tempdir().unwrap();
    let body = format!(
        "{{\"type\":\"mode\",\"mode\":\"normal\",\"sessionId\":\"{CLAUDE_ID}\"}}\n\
         {{\"type\":\"ai-title\",\"aiTitle\":\"Tmux plugin\",\"sessionId\":\"{CLAUDE_ID}\"}}\n\
         {{\"type\":\"user\",\"sessionId\":\"{CLAUDE_ID}\",\"message\":{{\"role\":\"user\",\
           \"content\":\"never used, an agent title outranks a prompt\"}}}}\n\
         {{\"type\":\"ai-title\",\"aiTitle\":\"Tmux memory management plugin\",\
           \"sessionId\":\"{CLAUDE_ID}\"}}\n"
    );
    let (adapter, session) = claude(tmp.path(), &body);

    let title = adapter
        .title_of(&session, Policy::AgentOrFirstPrompt)
        .expect("a transcript carrying an ai-title has a title");
    assert_eq!(
        title.text, "Tmux memory management plugin",
        "the last ai-title is the current one; the earlier ones are revisions \
         it replaced"
    );
    assert_eq!(
        title.source,
        Source::Agent,
        "an ai-title is the agent's own words about itself, and is labelled as \
         such: {title:?}"
    );
}

/// An `ai-title` names the conversation it belongs to. One naming a different
/// conversation is not this conversation's title, whatever file it turned up
/// in.
#[test]
fn a_title_belonging_to_another_conversation_is_not_used() {
    let tmp = tempfile::tempdir().unwrap();
    let body = "{\"type\":\"ai-title\",\"aiTitle\":\"Somebody else's work\",\
                \"sessionId\":\"11111111-2222-3333-4444-555555555555\"}\n";
    let (adapter, session) = claude(tmp.path(), body);

    assert_eq!(
        adapter.title_of(&session, Policy::AgentOrFirstPrompt),
        None,
        "a title attributed to another conversation must be refused, not shown \
         against this one"
    );
}

/// Most conversations have no `ai-title` at all — three of the newest ten on
/// the maintainer's machine do not — so the fallback is the first thing the
/// *user* typed, and everything the agent's own machinery writes as a user
/// message is skipped on the way to it.
#[test]
fn a_claude_conversation_without_a_title_falls_back_to_the_first_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let body = format!(
        "{{\"type\":\"user\",\"isMeta\":true,\"sessionId\":\"{CLAUDE_ID}\",\
           \"message\":{{\"role\":\"user\",\"content\":\"<local-command-caveat>Caveat: the \
           messages below were generated by the user while running local commands\
           </local-command-caveat>\"}}}}\n\
         {{\"type\":\"user\",\"sessionId\":\"{CLAUDE_ID}\",\"message\":{{\"role\":\"user\",\
           \"content\":\"<command-name>/clear</command-name><command-message>clear\
           </command-message>\"}}}}\n\
         {{\"type\":\"user\",\"isSidechain\":true,\"sessionId\":\"{CLAUDE_ID}\",\
           \"message\":{{\"role\":\"user\",\"content\":\"a subagent's brief, not the user's\"}}}}\n\
         {{\"type\":\"user\",\"sessionId\":\"{CLAUDE_ID}\",\"message\":{{\"role\":\"user\",\
           \"content\":[{{\"type\":\"text\",\"text\":\"Fix the restore race in osm\"}}]}}}}\n"
    );
    let (adapter, session) = claude(tmp.path(), &body);

    let title = adapter
        .title_of(&session, Policy::AgentOrFirstPrompt)
        .expect("a first prompt was typed");
    assert_eq!(
        title.text, "Fix the restore race in osm",
        "the caveat block, the slash command and the subagent brief are the \
         agent's machinery; the user's own first line is the one thing here \
         that says what the conversation is for"
    );
    assert_eq!(
        title.source,
        Source::FirstPrompt,
        "a prompt-derived title is transcript content and says so, so a reader \
         can tell it from a title the agent wrote: {title:?}"
    );
}

/// Codex writes no title of its own. Its rollout does record the moment the
/// user typed something — `event_msg` / `user_message` — and the role-`user`
/// messages before it are Codex's own preamble, not the user.
#[test]
fn a_codex_conversation_is_named_by_the_first_prompt_and_not_by_the_preamble() {
    let tmp = tempfile::tempdir().unwrap();
    let body = format!(
        "{{\"timestamp\":\"2026-08-24T10:00:00Z\",\"type\":\"session_meta\",\
           \"payload\":{{\"id\":\"{CODEX_ID}\",\"cwd\":\"/home/u/app\"}}}}\n\
         {{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"developer\",\
           \"content\":[{{\"type\":\"input_text\",\"text\":\"You are Codex, an agent\"}}]}}}}\n\
         {{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\
           \"content\":[{{\"type\":\"input_text\",\"text\":\"<recommended_plugins> Here is a \
           list of plugins that are available but not installed.\"}}]}}}}\n\
         {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\
           \"message\":\"Certify the one-way sync in proj-alpha\"}}}}\n"
    );
    let (adapter, session) = codex(tmp.path(), &body);

    let title = adapter
        .title_of(&session, Policy::AgentOrFirstPrompt)
        .expect("the rollout records what the user typed");
    assert_eq!(
        title.text, "Certify the one-way sync in proj-alpha",
        "the first role-user message in a Codex rollout is the plugin listing \
         Codex injects, and titling every conversation \"Here is a list of \
         plugins…\" is worse than titling none of them"
    );
    assert_eq!(title.source, Source::FirstPrompt);
}

/// A rollout whose own metadata names another conversation is not evidence
/// about this one.
#[test]
fn a_codex_rollout_that_names_another_conversation_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let body = "{\"type\":\"session_meta\",\"payload\":{\"id\":\
                \"11111111-2222-3333-4444-555555555555\"}}\n\
                {\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\
                \"message\":\"somebody else's prompt\"}}\n"
        .to_string();
    let (adapter, session) = codex(tmp.path(), &body);

    assert_eq!(
        adapter.title_of(&session, Policy::AgentOrFirstPrompt),
        None,
        "the conversation the file is named for and the one its metadata names \
         disagree, so nothing in it can be attributed to either"
    );
}

/// Nothing to say is said as nothing. The menu turns `None` into *untitled*;
/// what it must never be handed is a guess.
#[test]
fn a_conversation_with_neither_a_title_nor_a_prompt_has_none() {
    let tmp = tempfile::tempdir().unwrap();
    let body = format!(
        "{{\"type\":\"mode\",\"mode\":\"normal\",\"sessionId\":\"{CLAUDE_ID}\"}}\n\
         {{\"type\":\"assistant\",\"sessionId\":\"{CLAUDE_ID}\",\"message\":{{\"role\":\
           \"assistant\",\"content\":\"I will start by reading the file\"}}}}\n"
    );
    let (adapter, session) = claude(tmp.path(), &body);

    assert_eq!(
        adapter.title_of(&session, Policy::AgentOrFirstPrompt),
        None,
        "an assistant's words are not a title and are not the user's goal \
         either; there is nothing here to show"
    );
}

/// One line, bounded. A prompt is transcript content, and the whole of the
/// relaxation is that a *single truncated line* of it may be shown — never a
/// message body.
#[test]
fn a_prompt_derived_title_is_one_short_line() {
    let tmp = tempfile::tempdir().unwrap();
    let prompt = format!(
        "Rework the restore path so that\\n\\n  * every pane is placed\\n  * {}",
        "and then a great deal more prose that nobody wants in a bar popup ".repeat(20)
    );
    let body = format!(
        "{{\"type\":\"user\",\"sessionId\":\"{CLAUDE_ID}\",\"message\":{{\"role\":\"user\",\
           \"content\":\"{prompt}\"}}}}\n"
    );
    let (adapter, session) = claude(tmp.path(), &body);

    let title = adapter
        .title_of(&session, Policy::AgentOrFirstPrompt)
        .expect("a prompt was typed");
    assert!(
        !title.text.contains('\n') && !title.text.contains('\r'),
        "a title is one line: {:?}",
        title.text
    );
    assert!(
        title.text.chars().count() <= MAX_CHARS,
        "a title of {} characters is a message body on a bar popup, which is \
         exactly what must never be shown: {:?}",
        title.text.chars().count(),
        title.text
    );
    assert!(
        title.text.ends_with('…'),
        "a truncated title says it was truncated, so nobody reads it as the \
         whole of what was asked: {:?}",
        title.text
    );
    assert!(
        title.text.starts_with("Rework the restore path so that"),
        "the head of the prompt is the part that says what the work is: {:?}",
        title.text
    );
}

/// The bound is real, and this is what it costs: a title further from the end
/// of a transcript than the suffix osm reads is not found.
///
/// Written as an assertion rather than left implicit because it is the whole
/// safety property. The maintainer's largest transcript is 16 MB; reading all
/// of them to be certain of every title would be 1.4 GB of I/O for a bar
/// popup, and the last `ai-title` in every titled transcript measured on their
/// machine sits within 15 KB of the end.
#[test]
fn a_claude_title_beyond_the_bounded_suffix_is_not_read() {
    let tmp = tempfile::tempdir().unwrap();
    let filler = format!(
        "{{\"type\":\"assistant\",\"sessionId\":\"{CLAUDE_ID}\",\"pad\":\"{}\"}}\n",
        "x".repeat(4096)
    );
    let mut body = format!(
        "{{\"type\":\"ai-title\",\"aiTitle\":\"Buried far too deep\",\
           \"sessionId\":\"{CLAUDE_ID}\"}}\n"
    );
    // Comfortably past both the suffix scanned for a title and the prefix
    // scanned for a first prompt.
    while body.len() < 2 * 1024 * 1024 {
        body.push_str(&filler);
    }
    let (adapter, session) = claude(tmp.path(), &body);

    assert_eq!(
        adapter.title_of(&session, Policy::AgentOrFirstPrompt),
        None,
        "the title sits at the head of a 2 MB transcript, past the bounded \
         suffix; finding it would mean the read is not bounded at all"
    );
}

/// The same for Codex, whose bound is a prefix: the user's first message is
/// near the top, behind a preamble that can be hundreds of kilobytes.
#[test]
fn a_codex_prompt_beyond_the_bounded_prefix_is_not_read() {
    let tmp = tempfile::tempdir().unwrap();
    let mut body = format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{CODEX_ID}\"}}}}\n");
    let filler = format!(
        "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\
           \"developer\",\"content\":\"{}\"}}}}\n",
        "x".repeat(8192)
    );
    while body.len() < 2 * 1024 * 1024 {
        body.push_str(&filler);
    }
    body.push_str(
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\
         \"message\":\"typed after two megabytes of preamble\"}}\n",
    );
    let (adapter, session) = codex(tmp.path(), &body);

    assert_eq!(
        adapter.title_of(&session, Policy::AgentOrFirstPrompt),
        None,
        "the prompt is 2 MB into the rollout, past the bounded prefix; reading \
         it would mean 14 GB of rollouts are read in full whenever the menu \
         opens"
    );
}

/// A store path that no longer resolves is not a failure and not a title: the
/// file was replaced or deleted between discovery and here, which is an
/// ordinary race with a running agent.
#[test]
fn a_conversation_whose_transcript_has_gone_has_no_title() {
    let tmp = tempfile::tempdir().unwrap();
    let (adapter, session) = claude(
        tmp.path(),
        &format!("{{\"type\":\"ai-title\",\"aiTitle\":\"Gone\",\"sessionId\":\"{CLAUDE_ID}\"}}\n"),
    );
    fs::remove_file(session.store_path.as_deref().unwrap()).unwrap();

    assert_eq!(adapter.title_of(&session, Policy::AgentOrFirstPrompt), None);
}

/// `privacy.prompt_titles = false` and what it costs, said out loud.
///
/// Under [`Policy::AgentOnly`] nothing is derived from a message: a Claude
/// conversation keeps whatever its agent named it and loses the fallback, and
/// a Codex conversation — Codex writes no title of its own — is untitled,
/// which on the maintainer's machine is 2383 of 2494 conversations. That is
/// the deal the switch offers, and it is the reason it is not the default.
#[test]
fn the_strict_policy_takes_the_agents_own_title_and_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let (adapter, session) = claude(
        tmp.path(),
        &format!(
            "{{\"type\":\"ai-title\",\"aiTitle\":\"Named by the agent\",\
               \"sessionId\":\"{CLAUDE_ID}\"}}\n\
             {{\"type\":\"user\",\"sessionId\":\"{CLAUDE_ID}\",\"message\":{{\"role\":\
               \"user\",\"content\":\"and this is never read under the strict policy\"}}}}\n"
        ),
    );
    let title = adapter
        .title_of(&session, Policy::AgentOnly)
        .expect("the agent's own title is not a relaxation of anything");
    assert_eq!(title.text, "Named by the agent");
    assert_eq!(title.source, Source::Agent);
}

#[test]
fn the_strict_policy_leaves_a_conversation_with_no_agent_title_untitled() {
    let tmp = tempfile::tempdir().unwrap();
    let (claude_adapter, claude_session) = claude(
        tmp.path(),
        &format!(
            "{{\"type\":\"user\",\"sessionId\":\"{CLAUDE_ID}\",\"message\":{{\"role\":\
               \"user\",\"content\":\"Fix the restore race in osm\"}}}}\n"
        ),
    );
    assert_eq!(
        claude_adapter.title_of(&claude_session, Policy::AgentOnly),
        None,
        "the prompt is transcript content, and the strict policy does not read \
         transcript content"
    );

    let tmp2 = tempfile::tempdir().unwrap();
    let (codex_adapter, codex_session) = codex(
        tmp2.path(),
        &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{CODEX_ID}\"}}}}\n\
             {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\
               \"message\":\"Certify the one-way sync\"}}}}\n"
        ),
    );
    assert_eq!(
        codex_adapter.title_of(&codex_session, Policy::AgentOnly),
        None,
        "Codex writes no title of its own, so under the strict policy every \
         Codex conversation is untitled — 2383 of the 2494 on the maintainer's \
         machine"
    );
}

// ---------------------------------------------------------------------------
// The characters that are drawn as nothing, and the ones that reverse what
// follows them.
//
// `char::is_control` covers C0 and C1 — the escape sequences and the newlines
// a pasted prompt is full of — and stops there. Every Unicode *format*
// character survived it, and those are the ones that matter in a panel: U+202E
// RIGHT-TO-LEFT OVERRIDE reverses the run after it, the isolates at
// U+2066…U+2069 do the same job with a scope, and U+200B, U+FEFF and the tag
// characters at U+E0000 are drawn as nothing at all.
//
// A title is one line of somebody else's file rendered in the maintainer's
// bar. A row that reads one way and says another, or that hides half of itself,
// is the same defect as every other one this plugin is built around: the panel
// showing something that is not what osm recorded.
// ---------------------------------------------------------------------------

/// Every character a title must never carry, with what it does.
const HOSTILE: [(char, &str); 8] = [
    (
        '\u{202E}',
        "right-to-left override: reverses everything after it",
    ),
    ('\u{202D}', "left-to-right override"),
    ('\u{2066}', "left-to-right isolate"),
    ('\u{2069}', "pop directional isolate"),
    ('\u{200B}', "zero-width space: splits a word invisibly"),
    ('\u{FEFF}', "zero-width no-break space"),
    ('\u{00AD}', "soft hyphen: drawn as nothing"),
    ('\u{E0041}', "a tag character: an invisible second alphabet"),
];

#[test]
fn a_title_carries_no_character_that_can_reorder_or_hide_it() {
    for (ch, what) in HOSTILE {
        let raw = format!("rm -rf{ch} /home");
        let title = Title::new(&raw, Source::FirstPrompt)
            .unwrap_or_else(|| panic!("U+{:04X} ate the whole title", ch as u32));
        assert!(
            !title.text.chars().any(|c| c == ch),
            "U+{:04X} ({what}) survived into a title drawn in the panel: {:?}",
            ch as u32,
            title.text
        );
        assert_eq!(
            title.text, "rm -rf /home",
            "removing U+{:04X} must leave the rest of the line exactly as it \
             was: {:?}",
            ch as u32, title.text
        );
    }
}

/// The attack the override characters are for, spelled out.
///
/// Rendered with U+202E in it, `…202E gnp.eciovni` reads as `invoice.png` in
/// the panel while the title osm recorded says something else entirely. What
/// is drawn and what is stored have to be the same string.
#[test]
fn a_reversed_title_is_stored_and_drawn_as_the_same_string() {
    let title = Title::new("Open \u{202E}gnp.eciovni", Source::FirstPrompt).expect("a title");
    assert_eq!(title.text, "Open gnp.eciovni");
}

/// Concealment, which is the other half: an isolate pair or a zero-width space
/// can hide a whole clause from a reader who is looking straight at it.
#[test]
fn nothing_in_a_title_is_drawn_as_nothing() {
    let title = Title::new(
        "Deploy\u{2066} to production\u{2069}\u{200B}\u{2060} now",
        Source::FirstPrompt,
    )
    .expect("a title");
    assert_eq!(title.text, "Deploy to production now");
}

/// And the ones that are kept, deliberately.
///
/// U+FE0F is a variation selector: it chooses a presentation for the visible
/// character before it and can neither hide nor reorder anything. Stripping it
/// would change how an ordinary emoji in a title is drawn, for no safety at
/// all.
#[test]
fn a_variation_selector_is_left_alone() {
    let title = Title::new("Ship it \u{2764}\u{FE0F}", Source::Agent).expect("a title");
    assert_eq!(title.text, "Ship it \u{2764}\u{FE0F}");
}

/// A title that is nothing but hidden characters is no title, not an empty
/// one. A blank row reads as a session with no purpose; *untitled* is what
/// "osm could derive nothing" looks like.
#[test]
fn a_title_made_only_of_invisible_characters_is_no_title() {
    assert_eq!(
        Title::new("\u{200B}\u{FEFF}\u{202E}\u{2069}", Source::FirstPrompt),
        None
    );
}

/// End to end, through the adapter: what a hostile transcript actually
/// produces.
#[test]
fn an_agents_own_title_is_stripped_too() {
    let tmp = tempfile::tempdir().unwrap();
    let (adapter, session) = claude(
        tmp.path(),
        &format!(
            "{{\"type\":\"ai-title\",\"aiTitle\":\"Report \\u202Egnp.eciovni\",\
               \"sessionId\":\"{CLAUDE_ID}\"}}\n"
        ),
    );
    let title = adapter
        .title_of(&session, Policy::AgentOrFirstPrompt)
        .expect("a title");
    assert_eq!(
        title.text, "Report gnp.eciovni",
        "a title the agent wrote is transcript content like any other, and \
         goes through the same stripping"
    );
}
