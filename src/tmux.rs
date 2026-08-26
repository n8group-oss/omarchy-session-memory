use anyhow::{anyhow, Context, Result};
use std::fmt;
use std::process::Command;

/// The oldest tmux this engine will talk to, as `(major, minor)`.
///
/// # Why the floor is 3.7 and not something more forgiving
///
/// tmux ≤ 3.6 rewrites every byte of `-F` format output that it does not
/// consider printable ASCII — newlines, tabs, and **every byte of every
/// non-ASCII character** — to `_`, *before* the value reaches osm. That is not
/// a framing problem an escape scheme can solve: the substitution happens
/// inside the tmux server, so what arrives is already a different string.
///
/// A pane sitting in `/home/u/żółć` is therefore captured as `/home/u/______`
/// and *restored into that path* — a directory that either does not exist (the
/// pane silently comes back in `$HOME`, reported as degraded) or, worse,
/// exists and is not the user's. Window names, pane titles and command names
/// are corrupted the same way. No amount of quoting on this side recovers data
/// the server destroyed upstream, so the only honest answer is to refuse to
/// run: a loud failure at startup costs a user one message, and silent
/// corruption of every non-ASCII path costs them their state.
///
/// Debian bookworm ships 3.3a and Ubuntu ships 3.4/3.5a, so this genuinely
/// excludes people. See the README for what they have to do.
///
/// The version is only half of it: even tmux 3.7 sanitises the same way for a
/// client whose environment does not name a UTF-8 locale, which is the state a
/// systemd user unit starts in. [`Tmux::run`] passes `-u` on every invocation
/// for that half.
pub const MIN_VERSION: Version = Version { major: 3, minor: 7 };

/// A tmux version reduced to the two numbers that order it.
///
/// The trailing letter tmux appends to a release (`3.7c`) is a patch level
/// within the same feature set, so it is parsed and discarded rather than
/// compared: nothing this engine depends on has ever appeared in one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The version in a `tmux -V` line.
///
/// Accepts what tmux actually prints: `tmux 3.7c`, `tmux 3.3a`, `tmux 3.4`,
/// and the development builds' `tmux next-3.8`. Anything else — `tmux master`,
/// an OpenBSD build's `tmux openbsd-7.4`, a wrapper script printing something
/// of its own — is an error rather than an optimistic pass, because "we could
/// not tell how old this tmux is" is exactly the state in which the corruption
/// above happens unnoticed.
pub fn parse_version(reported: &str) -> Result<Version> {
    let raw = reported.trim();
    let rest = raw.strip_prefix("tmux ").unwrap_or(raw).trim();
    // Development snapshots of the *next* release: `next-3.8` is 3.8's
    // feature set, so it is read as 3.8.
    let rest = rest.strip_prefix("next-").unwrap_or(rest);
    let digits = |s: &str| -> (String, usize) {
        let taken: String = s.chars().take_while(char::is_ascii_digit).collect();
        let len = taken.len();
        (taken, len)
    };
    let (major, used) = digits(rest);
    let bad = || {
        anyhow!(
            "cannot tell which tmux version {reported:?} is; \
             osm requires tmux {MIN_VERSION} or newer and refuses to guess"
        )
    };
    if major.is_empty() || rest.as_bytes().get(used) != Some(&b'.') {
        return Err(bad());
    }
    let (minor, used_minor) = digits(&rest[used + 1..]);
    if minor.is_empty() {
        return Err(bad());
    }
    // Whatever follows the minor number may only be the release letter
    // (`3.7c`), never more digits or another dotted component.
    let tail = &rest[used + 1 + used_minor..];
    if !tail.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(bad());
    }
    Ok(Version {
        major: major.parse().map_err(|_| bad())?,
        minor: minor.parse().map_err(|_| bad())?,
    })
}

/// [`parse_version`], then the floor.
///
/// The message names the version that was actually found — an operator who is
/// told only "tmux is too old" has to go and look, and the whole point of
/// failing here is that they should not have to.
pub fn check_version(reported: &str) -> Result<Version> {
    let found = parse_version(reported)?;
    if found < MIN_VERSION {
        return Err(anyhow!(
            "this tmux is {found} ({}), and osm requires tmux {MIN_VERSION} or newer. \
             tmux {found} rewrites newlines and every non-ASCII byte in format output to \
             '_' before osm can read it, so a working directory such as \
             /home/u/\u{17c}\u{f3}\u{142}\u{107} would be captured under a different path \
             and restored into the wrong place. Debian bookworm and Ubuntu ship older \
             tmux; install tmux {MIN_VERSION} or newer (for example from \
             https://github.com/tmux/tmux/releases) before running osm.",
            reported.trim()
        ));
    }
    Ok(found)
}

/// The literal every framing token opens with, and the only sequence that has
/// to be escaped out of field values.
///
/// Both [`FIELD_SEP`] and [`REC_SEP`] start with it, so a value that cannot
/// contain this prefix cannot contain either separator.
const TOKEN_PREFIX: &str = "<|osm";

/// Field separator for every `tmux … -F` format this module issues.
///
/// # Why not an ASCII control character
///
/// This used to be the ASCII Unit Separator (`0x1f`), on the reasoning that a
/// non-printable byte can never occur in a path, window name or command line.
/// tmux disagrees: **tmux ≤ 3.6 replaces every non-printable byte in format
/// output with `_`**, so the separator never reached us. Verified
/// byte-for-byte on the same input:
///
/// ```text
/// tmux 3.7c:  $ 0 037 a l p h a      <- the real 0x1F survives
/// tmux 3.3a:  $ 0  _  a l p h a      <- replaced with an underscore
/// ```
///
/// The whole line then parses as one field and every capture fails with
/// `expected 2 fields, got 1`. tmux 3.3a is what Debian bookworm ships and
/// 3.4 is on Ubuntu, i.e. osm was silently broken for most users. Tabs and
/// non-ASCII (`U+241F ␟`) are mangled the same way, so the separator has to
/// be **printable ASCII**.
///
/// # Printable means collidable, so values are escaped
///
/// A printable token is ordinary text: `tmux rename-window 'work<|osm|>prod'`
/// or a directory called `/tmp/a<|osm|>b` used to make **every** capture from
/// then on fail its field-count check. Hoping the token is implausible is not
/// a defence, so nothing is left to hope: every field is escaped by tmux
/// itself before it is framed (see [`escaped`]) and decoded on the way back
/// in (see [`decode_field`]). No value tmux can report can contain
/// [`TOKEN_PREFIX`] by the time it reaches [`parse_records`].
pub const FIELD_SEP: &str = "<|osm:f|>";

/// Record separator, emitted at the **start** of every record.
///
/// Records used to be framed by newlines, which a value may contain: a
/// directory name may hold one (POSIX allows any byte but `/` and NUL), and
/// tmux ≥ 3.7 passes it through verbatim where 3.3a rewrote it to `_`. One
/// such directory shifted every following field by a line and failed the
/// capture.
///
/// Leading rather than trailing, so that tmux's own terminating newline is
/// always the **last** byte of the piece it belongs to and can be stripped
/// unambiguously — even when the record's final field legitimately ends in a
/// newline. See [`parse_records`].
pub const REC_SEP: &str = "<|osm:r|>";

/// The escape character introduced by [`encode_field`], chosen because it is
/// printable ASCII (so tmux ≤ 3.6 does not rewrite it), needs no escaping in
/// a POSIX regex, and is not special to tmux's format parser.
const ESC: char = '~';

/// Escape `s` exactly as the two nested tmux substitutions in [`escaped`] do.
///
/// The order matters and is the same on both sides: escape the escape
/// character first, then the token prefix. `~` is doubled and `<|osm` becomes
/// `~L`, so the output is a prefix-free code that [`decode_field`] reverses
/// with a single left-to-right scan.
///
/// Public because the parser tests build fixtures with it, and because it is
/// the executable specification of what tmux is being asked to do.
pub fn encode_field(s: &str) -> String {
    s.replace(ESC, "~~").replace(TOKEN_PREFIX, "~L")
}

/// Reverse [`encode_field`].
///
/// An escape character followed by anything else is an error rather than a
/// silent pass-through: it means the substitutions never ran (a tmux older
/// than 3.1, which has no `s///` format modifier), and a capture that quietly
/// recorded half-decoded paths would be worse than one that fails loudly.
fn decode_field(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != ESC {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('~') => out.push(ESC),
            Some('L') => out.push_str(TOKEN_PREFIX),
            Some(other) => {
                return Err(anyhow!(
                    "unknown escape {ESC}{other} in field {s:?}; \
                     tmux did not apply the escaping substitutions \
                     (osm requires tmux 3.7 or newer)"
                ))
            }
            None => {
                return Err(anyhow!(
                    "field {s:?} ends in a lone {ESC}; \
                     tmux did not apply the escaping substitutions \
                     (osm requires tmux 3.7 or newer)"
                ))
            }
        }
    }
    Ok(out)
}

/// The tmux format expression that emits `var` already escaped.
///
/// Two nested `s/pattern/replacement/` modifiers, applied inside-out: `~` is
/// doubled first, then `<|osm` is replaced by `~L`. `\|` is required because
/// tmux matches with POSIX **extended** regexes, where a bare `|` is
/// alternation.
///
/// Both substitutions are verified to work, globally and nested, on tmux
/// 3.3a, 3.5a and 3.7c. Escaping newlines the same way is deliberately *not*
/// attempted: a literal newline in the pattern matches the empty string on
/// tmux 3.3a and 3.5a, which inserts the replacement between every character
/// of the value. Newlines are handled by the framing instead — see
/// [`REC_SEP`].
fn escaped(var: &str) -> String {
    format!("#{{s/<\\|osm/~L/:#{{s/~/~~/:{var}}}}}")
}

/// The `-F` format string for a record made of `vars`, in order.
///
/// Every field is escaped, including the structured ones (ids, indices,
/// flags, layouts). Escaping a value that provably cannot contain the token
/// costs nothing and removes the whole class of "this one field was
/// forgotten" bugs — the decoder can then be applied uniformly.
pub fn record_format(vars: &[&str]) -> String {
    let mut out = String::from(REC_SEP);
    for (i, var) in vars.iter().enumerate() {
        if i > 0 {
            out.push_str(FIELD_SEP);
        }
        out.push_str(&escaped(var));
    }
    out
}

/// Split `out` into records of exactly `n` decoded fields.
///
/// Framing is by [`REC_SEP`] alone; newlines are ordinary data. tmux prints
/// one newline after each formatted record, which lands at the end of that
/// record's piece and is stripped there — exactly one, so a field whose value
/// really does end in a newline keeps it.
pub fn parse_records(out: &str, n: usize) -> Result<Vec<Vec<String>>> {
    let mut pieces = out.split(REC_SEP);
    // Anything before the first record separator is not part of any record.
    // tmux emits nothing there, so a non-empty head means the format string
    // was not expanded at all (an unsupported modifier on an ancient tmux)
    // and every "record" after it would be garbage.
    match pieces.next() {
        Some(head) if head.trim().is_empty() => {}
        Some(head) => {
            return Err(anyhow!(
                "unexpected output {head:?} before the first record separator; \
                 tmux did not expand the format string"
            ))
        }
        None => return Ok(Vec::new()),
    }

    let mut records = Vec::new();
    for piece in pieces {
        // tmux's own record-terminating newline, and nothing else.
        let body = piece.strip_suffix('\n').unwrap_or(piece);
        let parts: Vec<&str> = body.split(FIELD_SEP).collect();
        if parts.len() != n {
            return Err(anyhow!(
                "expected {n} fields, got {} in record {body:?}",
                parts.len()
            ));
        }
        records.push(
            parts
                .into_iter()
                .map(decode_field)
                .collect::<Result<Vec<String>>>()?,
        );
    }
    Ok(records)
}

/// Format fields, in order, for each list command. Structured fields (ids,
/// indices, flags) come first and free-text fields last, so a record is easy
/// to read in a debugger; correctness no longer depends on that order, since
/// every field is escaped.
const SESSION_FIELDS: [&str; 2] = ["session_id", "session_name"];
const WINDOW_FIELDS: [&str; 8] = [
    "session_id",
    "window_id",
    "window_index",
    "window_active",
    "window_zoomed_flag",
    // Not a `window_` format variable but a *window option*, which formats
    // resolve by its real, hyphenated name — `#{automatic_rename}` expands to
    // the empty string on every tmux tested. Documented under FORMATS as
    // `#{?automatic-rename,yes,no}`.
    "automatic-rename",
    "window_name",
    "window_layout",
];
const PANE_FIELDS: [&str; 9] = [
    "window_id",
    "pane_id",
    "pane_index",
    "pane_active",
    "pane_dead",
    "pane_pid",
    "pane_current_path",
    "pane_title",
    "pane_current_command",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRec {
    pub id: String,
    pub name: String,
}

/// One `list-windows -a` row: a window **as seen from one session**.
///
/// A window linked into several sessions produces one row per session, each
/// with that session's own `idx` and `active` flag but the same `id`. The
/// window's contents (name, layout, panes) are shared — see
/// [`crate::capture`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRec {
    pub session_id: String,
    pub id: String,
    pub idx: u32,
    pub name: String,
    pub layout: String,
    pub active: bool,
    pub zoomed: bool,
    /// Whether tmux owns this window's name (`automatic-rename` is on for it).
    ///
    /// When it is, [`Self::name`] is a *derived* value — tmux rewrites it from
    /// the foreground command, so a window created a moment ago reads `tmux`
    /// and settles to `bash` shortly after. Recorded so equivalence can tell a
    /// name the user chose from one tmux is still editing; see
    /// [`crate::equiv::difference`].
    ///
    /// tmux turns the flag off by itself the moment a name is given — by
    /// `rename-window`, or by `-n` at creation — so "off" really does mean
    /// "somebody said this window is called that".
    pub auto_named: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRec {
    pub window_id: String,
    pub id: String,
    pub idx: u32,
    pub active: bool,
    pub dead: bool,
    /// The pid of the process leading the pane (`#{pane_pid}`), used to walk
    /// its process tree and open file descriptors when detecting which
    /// agent conversation, if any, the pane is running — see
    /// `crate::agent::detect`.
    pub pid: u32,
    pub cwd: String,
    pub title: String,
    pub cmd: String,
}

fn flag(s: &str) -> bool {
    s == "1"
}

pub fn parse_sessions(out: &str) -> Result<Vec<SessionRec>> {
    parse_records(out, SESSION_FIELDS.len())?
        .into_iter()
        .map(|f| {
            let mut f = f.into_iter();
            Ok(SessionRec {
                id: f.next().expect("field 0"),
                name: f.next().expect("field 1"),
            })
        })
        .collect()
}

pub fn parse_windows(out: &str) -> Result<Vec<WindowRec>> {
    parse_records(out, WINDOW_FIELDS.len())?
        .into_iter()
        .map(|f| {
            Ok(WindowRec {
                session_id: f[0].clone(),
                id: f[1].clone(),
                idx: f[2].parse().context("window index")?,
                active: flag(&f[3]),
                zoomed: flag(&f[4]),
                auto_named: flag(&f[5]),
                name: f[6].clone(),
                layout: f[7].clone(),
            })
        })
        .collect()
}

pub fn parse_panes(out: &str) -> Result<Vec<PaneRec>> {
    parse_records(out, PANE_FIELDS.len())?
        .into_iter()
        .map(|f| {
            Ok(PaneRec {
                window_id: f[0].clone(),
                id: f[1].clone(),
                idx: f[2].parse().context("pane index")?,
                active: flag(&f[3]),
                dead: flag(&f[4]),
                pid: f[5].parse().context("pane pid")?,
                cwd: f[6].clone(),
                title: f[7].clone(),
                cmd: f[8].clone(),
            })
        })
        .collect()
}

/// A tmux server to talk to.
///
/// The socket is **private and has no public socket-less constructor by
/// default**: the only way to build one from outside this crate is
/// [`Tmux::with_socket`], so no test can construct a `Tmux` pointing at the
/// developer's default server — not even by writing the struct literal.
/// Reaching the default server once destroyed a live 7-session development
/// environment; the type system now forbids it rather than a CI grep asking
/// nicely.
///
/// [`Tmux::default_server`] exists only under the `default-server` feature
/// (on by default so the shipped binary works with no `--socket`). Test
/// runs build with `--no-default-features`, where that constructor does not
/// exist at all.
#[derive(Debug, Clone)]
pub struct Tmux {
    socket: Option<String>,
}

/// The tmux server option this engine keeps a server's identity in.
///
/// A user option (`@`-prefixed) at **server** scope: it lives in the running
/// server's memory, is never written to disk, and cannot outlive the process
/// that holds it — which is precisely the lifetime the identity has to have.
const SERVER_ID_OPTION: &str = "@osm-server-id";

/// 128 bits, written as hex.
const SERVER_ID_HEX_LEN: usize = 32;

/// Distinguishes the delivery witnesses one process places, so two
/// concurrent guarded runs cannot overwrite each other's.
static WITNESS_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 128 fresh bits from the kernel, as lowercase hex.
///
/// `/dev/urandom` rather than a crate: this needs to be unguessable-by-luck,
/// not cryptographic, and the project takes no new dependencies. A short read
/// is an error — a truncated id would be an identity two servers could share,
/// which is the whole hazard.
fn fresh_server_id() -> Result<String> {
    use std::io::Read;
    let mut buf = [0u8; SERVER_ID_HEX_LEN / 2];
    std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom for a server identity")?
        .read_exact(&mut buf)
        .context("read a server identity from /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// The tick at which process `pid` started, from `/proc/<pid>/stat` field 22.
///
/// # Why the identity needs a fact the option cannot carry
///
/// The random half of a server's identity lives in a tmux *option*, and an
/// option is only as trustworthy as the thing that set it. `set-option -s
/// @osm-server-id 0123456789abcdef0123456789abcdef` in a `.tmux.conf` — or in
/// a dump of server options someone restores — is structurally perfect and is
/// applied to **every** server the user ever starts, so every one of them
/// would report the same identity. A restore's window map would then still be
/// believed after a within-boot tmux restart, its reissued `@N`s would satisfy
/// the stale mappings, and the only snapshot that still knew two sessions
/// shared a window could be discharged and pruned.
///
/// So the identity is not the option's value; it is the option's value
/// **bound to a fact about this particular process that cannot be written into
/// a config file**. The kernel assigns a process's start tick when it forks it,
/// nothing can set it, and it is far finer-grained than the `tv_sec` tmux is
/// willing to report about itself: two servers started on the same socket a
/// few milliseconds apart already differ here even if every other field —
/// including a copied option — is identical.
///
/// A `/proc` entry that cannot be read or parsed is an error rather than a
/// fallback: an identity missing the half that makes it unforgeable is not a
/// weaker identity, it is the forgeable one.
/// The `@osm-server-id` value and the pid embedded in an incarnation token
/// minted by [`Tmux::server_incarnation`], which formats them as
/// `boot:id:pid:ticks:socket`.
///
/// The socket path is taken as everything after the fourth colon, because a
/// path may contain one; the four fields before it may not (a boot id and the
/// server id are hex, and the pid and ticks are digits).
fn incarnation_id_and_pid(token: &str) -> Option<(&str, &str)> {
    let mut fields = token.splitn(5, ':');
    let _boot = fields.next()?;
    let id = fields.next()?;
    let pid = fields.next()?;
    let _ticks = fields.next()?;
    let _socket = fields.next()?;
    let hex = id.len() == SERVER_ID_HEX_LEN && id.bytes().all(|b| b.is_ascii_hexdigit());
    let digits = !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit());
    (hex && digits).then_some((id, pid))
}

fn process_start_ticks(pid: u32) -> Result<u64> {
    let path = format!("/proc/{pid}/stat");
    let stat = std::fs::read_to_string(&path)
        .with_context(|| format!("read {path} for the tmux server's start time"))?;
    // Field 2 is the executable name in parentheses and may itself contain
    // spaces and parentheses, so the split point is the *last* ')': what
    // follows is field 3 onwards, which makes `starttime` (field 22) the
    // twentieth of them.
    let after_comm = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest)
        .ok_or_else(|| anyhow!("{path} is not in the form /proc/<pid>/stat has: {stat:?}"))?;
    after_comm
        .split_whitespace()
        .nth(19)
        .and_then(|field| field.parse().ok())
        .ok_or_else(|| anyhow!("{path} does not carry a start time osm can read: {stat:?}"))
}

impl Tmux {
    pub fn with_socket(name: &str) -> Self {
        Self {
            socket: Some(name.to_string()),
        }
    }

    /// Talk to the **default** tmux server — the user's real one.
    ///
    /// Gated behind the `default-server` feature precisely so that a test
    /// binary compiled without it cannot call this, by construction.
    #[cfg(feature = "default-server")]
    pub fn default_server() -> Self {
        Self { socket: None }
    }

    /// The `-L` socket name this instance targets, if any.
    pub fn socket(&self) -> Option<&str> {
        self.socket.as_deref()
    }

    pub fn run(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("tmux");
        // `-u` on **every** invocation, and it is as load-bearing as the
        // version floor.
        //
        // tmux decides whether to hand a command client raw UTF-8 or to
        // sanitise it by looking at that client's own `LC_ALL` / `LC_CTYPE` /
        // `LANG` for the substring "UTF-8". If none of them has it, the server
        // rewrites every non-ASCII byte — and every newline — in `-F` output to
        // `_` before the client ever sees it. On tmux 3.7c, verbatim:
        //
        // ```text
        //     tmux -L p list-panes -a -F '#{pane_current_path}'   ->  /tmp/____
        //  tmux -u -L p list-panes -a -F '#{pane_current_path}'   ->  /tmp/żółć
        // ```
        //
        // So the corruption the 3.7 floor exists to prevent came straight back
        // on a *supported* tmux the moment osm ran without a UTF-8 locale in
        // its environment — which is the ordinary state of a systemd user unit
        // and of a tmux hook, i.e. of nearly every capture this engine makes.
        // `-u` says "this client handles UTF-8" and takes the decision away
        // from an environment variable nobody sets on purpose.
        cmd.arg("-u");
        if let Some(sock) = &self.socket {
            cmd.arg("-L").arg(sock);
        }
        cmd.args(args);
        let out = cmd
            .output()
            .with_context(|| format!("spawn tmux {args:?}"))?;
        if !out.status.success() {
            return Err(anyhow!(
                "tmux {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Which **incarnation** of a tmux server this handle is talking to.
    ///
    /// Not "which socket": a socket name is reused by every server ever
    /// started on it, and each of those servers hands out `@0`, `@1`, … from
    /// zero again. A window identity established against one of them
    /// therefore says nothing about the next one, which is how a restore's
    /// captured→live window map could still be believed by a capture taken
    /// after a within-boot tmux restart — with a window name and a pane count
    /// the only things left between a carried session and an unrelated
    /// window.
    ///
    /// # Why this is a random number and not the server's own facts
    ///
    /// The obvious identity is the tuple every server can report about
    /// itself — `{boot}:{pid}:{start_time}:{socket_path}` — and it is
    /// **reusable**. tmux keeps its start time as a `timeval`, but the format
    /// layer emits only `tv_sec`, so the subsecond half never leaves the
    /// server; a pid is reused within a boot, which a small `pid_max` and a
    /// little process churn force on demand; and the socket path is fixed by
    /// the `-L` name. Two servers started in the same second onto the same
    /// reused pid therefore produce a byte-identical token, and a dead
    /// server's window map is then accepted against the live one — reissued
    /// `@N`s and all.
    ///
    /// So the identity is 128 random bits from the kernel, minted once per
    /// server and kept in a tmux **server option**, which lives in that
    /// server's memory and dies with it. `set-option -s -o` is the create
    /// half: `-o` refuses an option that is already set, and tmux processes
    /// commands one at a time, so two osm processes racing to name the same
    /// fresh server cannot both win — the loser reads the winner's value and
    /// agrees with it. The boot id and socket path are kept in the token for
    /// the person reading the database; they carry none of its uniqueness.
    /// Note that `#{start_time}` is *not* in the token — the pid's start tick
    /// below is the same fact at a hundred times the resolution, and it is
    /// read from the kernel rather than from the server being identified.
    ///
    /// # Why the option alone is not the identity
    ///
    /// An option is only as trustworthy as whatever set it, and this one is
    /// trivially set by a `.tmux.conf` line or a restored server-option dump —
    /// which would hand *every* server the user starts the same, perfectly
    /// well-formed identity. So the random value is **bound** to
    /// [`process_start_ticks`], a fact the kernel assigns to this particular
    /// server process and that nothing can write into a config file. Two
    /// consecutive servers preloaded with the same option are still distinct
    /// incarnations, because their start ticks are.
    ///
    /// The pid, start ticks, boot id and socket path together are what makes
    /// the token readable to a person looking at the database; the random half
    /// is what makes it unguessable, and the start ticks are what make it
    /// unforgeable. None of the three is load-bearing alone.
    ///
    /// Requires a running server — `display-message` has nothing to answer
    /// otherwise — and refuses to invent one: an id that is not 32 hex
    /// characters, a pid that did not expand, a socket path that did not
    /// expand, or a `/proc` entry that will not say when the server started,
    /// each make this an error rather than an identity several servers could
    /// share. An error here is **not** "no identity": see
    /// [`Tmux::running_server_incarnation`], which is what callers use to keep
    /// "there is no server" apart from "this server cannot be identified".
    pub fn server_incarnation(&self) -> Result<String> {
        let (id, pid, socket) = self.read_identity()?;
        let id = if id.is_empty() {
            // Create-once. A failure here is not fatal on its own: it is what
            // a lost race looks like, and the read below is the arbiter
            // either way.
            let _ = self.run(&[
                "set-option",
                "-s",
                "-o",
                SERVER_ID_OPTION,
                &fresh_server_id()?,
            ]);
            self.read_identity()?.0
        } else {
            id
        };
        let hex =
            |s: &str| s.len() == SERVER_ID_HEX_LEN && s.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex(&id) {
            return Err(anyhow!(
                "the tmux server option {SERVER_ID_OPTION} holds {id:?}, which is not \
                 something osm minted. osm keeps this server's identity there and \
                 cannot tell one server from another without it, so it will not \
                 capture or restore against this server; remove \
                 `set-option -s {SERVER_ID_OPTION}` from your tmux configuration."
            ));
        }
        let Ok(pid_num) = pid.parse::<u32>() else {
            return Err(anyhow!(
                "tmux did not report its server pid: pid={pid:?} \
                 (osm needs it to bind {SERVER_ID_OPTION} to this server process)"
            ));
        };
        if socket.is_empty() {
            return Err(anyhow!(
                "tmux did not report a usable server identity: socket_path={socket:?}"
            ));
        }
        let ticks = process_start_ticks(pid_num)?;
        Ok(format!(
            "{}:{id}:{pid_num}:{ticks}:{socket}",
            crate::boot::current_boot_id()?
        ))
    }

    /// The identity of the server on this socket, or `None` when **no server
    /// is running at all**.
    ///
    /// The distinction this draws is the whole point of it. "There is no
    /// server" is an ordinary, legitimate state: it is what a boot restore
    /// starts from and what a machine with tmux closed looks like. "A server
    /// is running and cannot be identified" is a hard failure, because every
    /// durable thing osm writes — a snapshot's rows, a restore's window map —
    /// is a statement about one server incarnation, and a `$0`/`@0`/`%0` that
    /// belongs to no named incarnation belongs to whichever server is asked
    /// next.
    ///
    /// Collapsing the two into `None` is what let an unusable identity mean
    /// "no server", which in turn let a mid-restore restart pass unnoticed and
    /// let a session list from one server be written down beside another's
    /// windows.
    pub fn running_server_incarnation(&self) -> Result<Option<String>> {
        match self.server_incarnation() {
            Ok(id) => Ok(Some(id)),
            // Only a server that is genuinely not there turns an error into
            // `None`; anything else propagates.
            Err(_) if !self.server_running() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// This server's stored id (empty when it has none yet), the server
    /// process's pid, and its socket path, read in one round trip.
    ///
    /// Separated by tabs, which none of the three can contain: the id is hex,
    /// the pid is digits, and the socket path is the last field, so a path
    /// with anything unusual in it cannot be mistaken for a further separator.
    fn read_identity(&self) -> Result<(String, String, String)> {
        let raw = self.run(&[
            "display-message",
            "-p",
            &format!("#{{{SERVER_ID_OPTION}}}\t#{{pid}}\t#{{socket_path}}"),
        ])?;
        let line = raw.trim_end_matches('\n');
        let mut fields = line.splitn(3, '\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some(id), Some(pid), Some(socket)) => {
                Ok((id.to_string(), pid.to_string(), socket.to_string()))
            }
            _ => Err(anyhow!(
                "tmux did not report a server identity line: {line:?}"
            )),
        }
    }

    /// Run `command` — one tmux command, in tmux's own syntax — **only if**
    /// this server is still the incarnation `incarnation` names, with the
    /// check and the command evaluated together inside the server.
    ///
    /// # Why the check cannot be a separate call
    ///
    /// Reading the identity and then acting on the answer leaves a gap, and
    /// the thing on the other side of that gap is a resume command carrying
    /// somebody's conversation. A server that dies after the read and is
    /// replaced before the send hands the replacement the same socket, and
    /// the replacement mints `%0`, `%1`, … from zero — so the pane id the
    /// caller verified now names an unrelated pane belonging to whatever the
    /// user has started since. Noticing afterwards is not a remedy: the input
    /// has been delivered.
    ///
    /// `if-shell -F` closes it. The condition is a format, so tmux evaluates
    /// it in the server process and runs `command` in the same command
    /// invocation, with no window in between for a different server to answer
    /// in. A server that is *not* the named incarnation cannot run the
    /// command, and a socket whose server has gone away cannot run anything
    /// at all.
    ///
    /// # Why the condition is a witness and not the identity's own fields
    ///
    /// The condition used to compare the two fields of the token tmux can
    /// read about itself — the `@osm-server-id` option and the pid — and that
    /// is a *partial* identity standing in for a whole one. An incarnation is
    /// the option **bound to the process start tick** (see
    /// [`process_start_ticks`]), precisely because the option alone is
    /// forgeable: a `.tmux.conf` line, or a restored dump of server options,
    /// hands every server the user starts the same well-formed id. A
    /// replacement server carrying such a configured id, onto a pid the kernel
    /// has since reissued, satisfied both halves — and received the resume
    /// into its own freshly-minted `%N`. A tmux format cannot read `/proc`, so
    /// the missing half cannot simply be added to the condition.
    ///
    /// What can be put in the condition is something the recorded server holds
    /// and no later server can: 128 fresh bits from the kernel, written into a
    /// server option of its own before the identity is checked, and named
    /// uniquely per call so two concurrent deliveries cannot overwrite each
    /// other's. The order is what makes it sound:
    ///
    /// 1. the witness is written to whatever server is on the socket;
    /// 2. the **full** incarnation is read back and compared, start tick and
    ///    all. A server on a socket is never succeeded by an earlier one, so a
    ///    server that reads back as the recorded incarnation now is the server
    ///    step 1 wrote to;
    /// 3. the guarded command runs only where that witness is, which after
    ///    step 2 is the recorded incarnation and nowhere else. A replacement
    ///    cannot hold it: it is random, is minted after the replacement's
    ///    configuration was written, and is never persisted anywhere.
    ///
    /// The id and the pid stay in the condition alongside it. They carry none
    /// of its uniqueness; they make the refusal legible to somebody reading
    /// the command tmux was asked to run.
    ///
    /// Returns `Ok(())` whether or not the condition held, and also when the
    /// identity read in step 2 shows the server has moved: `if-shell` with a
    /// false condition and no else-branch is a successful no-op, and so is
    /// declining to ask. Callers must therefore confirm the *effect* rather
    /// than the exit status, which is what [`crate::agent::resume::deliver`]
    /// does by waiting for the agent to appear and re-reading the identity if
    /// it never does.
    pub fn run_if_incarnation(&self, incarnation: &str, command: &str) -> Result<()> {
        let (id, pid) = incarnation_id_and_pid(incarnation).ok_or_else(|| {
            anyhow!("{incarnation:?} is not a tmux server incarnation token osm minted")
        })?;
        // Unique per call, so a second delivery running at the same moment
        // sets its own witness rather than overwriting this one — which would
        // turn a sound delivery into a silent refusal.
        let witness_option = format!(
            "@osm-delivery-{}-{}",
            std::process::id(),
            WITNESS_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let witness = fresh_server_id()?;
        self.run(&["set-option", "-s", &witness_option, &witness])
            .with_context(|| format!("place a delivery witness for {incarnation}"))?;
        let guarded = (|| -> Result<()> {
            // Step 2. Anything but the whole recorded incarnation — a
            // different server, no server, a server that will not identify
            // itself — means the witness above may have landed somewhere else,
            // so nothing is asked of anyone.
            if self.server_incarnation()? != incarnation {
                return Ok(());
            }
            let condition = format!(
                "#{{&&:#{{==:#{{{SERVER_ID_OPTION}}},{id}}},\
                 #{{&&:#{{==:#{{pid}},{pid}}},#{{==:#{{{witness_option}}},{witness}}}}}}}"
            );
            self.run(&["if-shell", "-F", &condition, command])?;
            Ok(())
        })();
        // Best effort, and only ever about this server's memory: a witness
        // left behind names no incarnation but the one that is holding it, and
        // dies with it.
        let _ = self.run(&["set-option", "-su", &witness_option]);
        guarded
    }

    pub fn server_running(&self) -> bool {
        self.run(&["list-sessions", "-F", "#{session_id}"]).is_ok()
    }

    /// What `tmux -V` reports, verbatim.
    ///
    /// `-V` does not need a running server, so this works before anything has
    /// been started on the socket — which is the state the boot restore finds.
    pub fn version_string(&self) -> Result<String> {
        Ok(self.run(&["-V"]).context("run tmux -V")?.trim().to_string())
    }

    /// Refuse to go on against a tmux older than [`MIN_VERSION`].
    ///
    /// Called once, at startup, by every subcommand that touches tmux. It is a
    /// hard failure rather than a warning: see [`MIN_VERSION`] for what an
    /// older server does to a non-ASCII path on the way *into* a snapshot,
    /// which is damage no later code can undo.
    pub fn require_supported_version(&self) -> Result<Version> {
        check_version(&self.version_string()?)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRec>> {
        parse_sessions(&self.run(&["list-sessions", "-F", &record_format(&SESSION_FIELDS)])?)
    }

    /// Every (session, window) pair on the server. A window linked into
    /// several sessions appears once per session.
    pub fn list_windows(&self) -> Result<Vec<WindowRec>> {
        parse_windows(&self.run(&["list-windows", "-a", "-F", &record_format(&WINDOW_FIELDS)])?)
    }

    /// Every pane on the server. A pane in a linked window is emitted once
    /// per link, so callers must deduplicate by `(window_id, pane_id)`.
    pub fn list_panes(&self) -> Result<Vec<PaneRec>> {
        parse_panes(&self.run(&["list-panes", "-a", "-F", &record_format(&PANE_FIELDS)])?)
    }
}
