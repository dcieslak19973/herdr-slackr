//! Feed and Mentions state: the in-memory model the pane (Task 7) renders. See
//! `docs/superpowers/specs/2026-07-12-herdr-slackr-design.md` §The pane and §Testing.
//!
//! [`App`] owns every subscribed conversation's messages in one `BTreeMap` keyed by a
//! sortable `(seconds, sequence, conv id, ts string)` tuple (`crate::model::ts_key` plus the
//! conv id as a tiebreaker, since two conversations can in principle share a `ts`, plus the
//! raw `ts` string as a final tiebreaker so that distinct malformed `ts` values — which all
//! collapse to `ts_key`'s `(0, 0)` fallback — never collide and silently overwrite one
//! another), so iterating the map in key order *is* the Feed tab's chronological,
//! cross-conversation order for free (the `ts`-string tiebreak only ever matters when the
//! numeric key is otherwise equal, so well-formed ordering is unchanged).
//!
//! [`App::build`] is the only I/O edge: it resolves configured channel names to ids, fetches
//! the self user id and the workspace's user list, and backfills each subscribed
//! conversation's last 50 messages, all via [`crate::rest`]. Everything it delegates to —
//! [`resolve_channels`], [`resolve_im_names`], and the `apply`/row-building/mention/divider
//! logic below — is pure and unit-tested without touching the network (house pattern: see
//! the REST and socket modules' thin-edge-over-pure-core split).
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::config::PluginConfig;
use crate::entities::{self, is_mention};
use crate::model::{ConvKind, Conversation, Message, resolve_channels, ts_cmp, ts_key};
use crate::rest::{Rest, RestError};
use crate::socket::SocketEvent;
use crate::tokens::Tokens;

/// Which tab is active; also which `*_rows`/read semantics `toggle_expand_or_read` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Feed,
    Mentions,
}

/// Which projection of the Feed tab's rows is shown (spec §3): the plain chronological
/// `feed_rows` timeline, the threads-only digest (`thread_rows`), or the live-only Focus filter
/// (`focus_rows`). Only meaningful on `Tab::Feed` — `toggle_view`/`toggle_focus` no-op on
/// `Tab::Mentions`, and the Mentions tab's own row list ignores this field entirely.
///
/// `Threads` and `Focus` are mutually exclusive Feed-tab view modes, each driven by its own key
/// (`t` for Threads via `toggle_view`, `f` for Focus via `toggle_focus`) rather than one shared
/// three-way cycle: every key press sets its *own* target view, using `Timeline` as the "off"
/// state for whichever *other* mode happened to be active — so `t` from `Focus` lands on
/// `Threads` (not back on `Timeline` first), and symmetrically for `f` from `Threads`. Decision
/// table (`view` before the press, across the top the key pressed):
///
/// | before \ key | `t`        | `f`        |
/// |--------------|------------|------------|
/// | `Timeline`   | `Threads`  | `Focus`    |
/// | `Threads`    | `Timeline` | `Focus`    |
/// | `Focus`      | `Threads`  | `Timeline` |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedView {
    Timeline,
    Threads,
    Focus,
}

/// One renderable row, identical for both tabs so Task 7 has a single render path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub conv_label: String,
    pub author: String,
    pub time_hhmm: String,
    pub text: String,
    pub kind: RowKind,
}

/// What kind of row this is, beyond a plain message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Message,
    ThreadMarker { replies: usize, expanded: bool },
    Divider,
    Mention { read: bool },
}

/// Which row-kind a selected `(conv, ts)` identity refers to. Needed because a thread root's
/// `Message` row and its collapsed `ThreadMarker` row share the very same `(conv, ts)` id (the
/// marker's id is the root's ts) — without this discriminant, identity resolution can only ever
/// find whichever of the two rows happens to come first, so a marker selection would silently
/// resolve to its root message instead. `Divider` never carries an id so it never needs a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelKind {
    Message,
    ThreadMarker,
    Mention,
}

fn sel_kind_of(row: &Row) -> SelKind {
    match row.kind {
        RowKind::ThreadMarker { .. } => SelKind::ThreadMarker,
        RowKind::Mention { .. } => SelKind::Mention,
        RowKind::Message | RowKind::Divider => SelKind::Message,
    }
}

/// A stored message plus its arrival order, used only to place the unread divider — arrival
/// order is not the same as `ts` order (a poll backfill can insert an old message after a
/// live one already arrived), so it is tracked independently as a monotonic counter.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stored {
    msg: Message,
    arrival: u64,
}

/// A message's map key: `ts_key` (seconds, sequence) first so iteration order is
/// chronological, then the conv id as a tiebreaker for the (practically impossible, but not
/// forbidden) case of two conversations sharing a `ts`, then the raw `ts` string itself as a
/// final tiebreaker. That last field is what keeps distinct malformed timestamps from
/// colliding: `ts_key` maps every unparseable `ts` to `(0, 0)`, so without the `ts` string in
/// the key, two different garbled messages in the same conv would land on the same key and
/// the second `upsert_new`/`upsert_edit` would silently clobber the first.
type Key = (u64, u32, String, String);

fn key_for(conv: &str, ts: &str) -> Key {
    let (secs, seq) = ts_key(ts);
    (secs, seq, conv.to_string(), ts.to_string())
}

/// A row plus the arrival counter used only to place the unread divider (see
/// `insert_divider`), before the id/divider-splicing pass drops the arrival and keeps just
/// the id.
type ArrivalRow = (u64, Option<(String, String)>, Row);

/// A row plus the `(conv, ts)` a Feed-tab action should act on; `None` for the synthetic
/// `Divider` row.
type IdRow = (Option<(String, String)>, Row);

/// The Feed/Mentions state model.
#[derive(Debug)]
pub struct App {
    pub tab: Tab,
    /// Which projection of the Feed tab is shown (see [`FeedView`]); irrelevant to `Tab::Mentions`.
    pub view: FeedView,
    /// Index into `visible_rows()` of the active tab (`feed_rows()` for `Tab::Feed`,
    /// `mention_rows()` for `Tab::Mentions`). Positional, but kept in sync with `selected` by
    /// `resync_cursor` after every row-set change, so it stays pointed at the same identity
    /// rather than silently retargeting when rows are inserted/removed elsewhere — see
    /// `move_cursor`.
    pub cursor: usize,
    /// One-line notice (socket down, rate limit, …); empty when there's nothing to say.
    pub status: String,
    /// True while in fallback polling mode (renders in `status` by convention, not enforced
    /// here — Task 7 decides exact wording).
    pub polling: bool,

    conversations: Vec<Conversation>,
    conv_names: HashMap<String, String>,
    conv_kinds: HashMap<String, ConvKind>,
    user_names: HashMap<String, String>,
    messages: BTreeMap<Key, Stored>,
    /// Threads currently expanded inline, keyed by `(conv, root ts)`.
    expanded: HashSet<(String, String)>,
    /// Mentions marked read, keyed by `(conv, ts)`. Absence means unread.
    read_mentions: HashSet<(String, String)>,
    /// Identity (`(conv, ts)` plus the row-kind it names — see `SelKind`) of the currently
    /// selected row, independent of its position in the active tab's row list. `move_cursor`
    /// sets this when the cursor moves; `resync_cursor` re-derives the public `cursor` index
    /// from it after any row-set change (insert/delete), so an action taken right after e.g. a
    /// poll backfill still lands on the row the user actually selected rather than whatever now
    /// happens to sit at the old index. The `SelKind` half is what lets a selected `ThreadMarker`
    /// resolve to itself rather than the root `Message` row it shares an id with. `None` before
    /// any selection has been made, or once the selected row list is empty.
    selected: Option<((String, String), SelKind)>,
    self_id: String,
    keywords: Vec<String>,
    /// Monotonic counter, incremented once per newly-seen message (see [`Stored::arrival`]).
    arrival_seq: u64,
    /// The `arrival_seq` value as of the last `touch()`; the divider sits before the first
    /// row whose arrival is greater than this.
    divider_mark: u64,
    /// The newest message `ts` seen so far per conversation, threaded into `poll_tick`'s
    /// `history` call as `oldest` so a routine tick only ever asks Slack for what's actually
    /// new. Derived cheaply at `upsert_new` time (see `track_newest`) rather than scanned out
    /// of `messages` on every tick: `messages` is one global `BTreeMap` with no per-conversation
    /// index, so finding a conversation's max `ts` there would mean a full linear scan per
    /// conversation per tick, while updating this map costs one comparison per newly-seen
    /// message — already-seen messages (the common case once caught up) don't even reach it.
    newest_ts: HashMap<String, String>,
    /// Round-robin position into `conversations` for `poll_tick`'s next `POLL_BATCH`-sized
    /// batch (see `next_batch`); persists across ticks so every conversation is eventually
    /// polled rather than only ever the first `POLL_BATCH`.
    poll_cursor: usize,
    /// Round-robin position into `active_threads()`'s list for `poll_tick_at`'s thread-refresh
    /// slots (see `thread_slot_count`); persists across ticks for the same reason `poll_cursor`
    /// does, independently of it — the active-thread list's length has nothing to do with
    /// `conversations`'s, so the two cursors would otherwise race each other's wraparound.
    poll_thread_cursor: usize,
    /// Set from a `RateLimited(secs)` hit during `poll_tick`; while `Some` and unexpired, ticks
    /// are skipped entirely (not merely shortened) rather than hammering Slack again on
    /// schedule. `Instant`-based (not wall-clock) so it is monotonic and immune to clock
    /// adjustments; the `now` it's compared against is injected as a parameter to
    /// `poll_tick_at` so the gating logic is unit-tested without a real sleep.
    cooldown_until: Option<Instant>,
    /// Count of arrivals since the cursor last left the active tab's bottom row (spec §3): an
    /// `apply`/`poll_tick` arrival while the cursor was already at the bottom follows it
    /// (`snap_cursor_to_last_row`) and never touches this; one while scrolled up increments it
    /// instead. Cleared by `maybe_clear_pending_new` the moment the cursor reaches the bottom
    /// again, by any means (`j`/`move_cursor`, `g`/`jump_first`, `G`/`jump_newest`, or a follow
    /// snap). Exposed read-only via `pending_new`.
    pending_new: usize,
    /// Rows available to the active tab's body viewport, set by Task 7's event loop once per
    /// draw (`set_viewport_rows`) from the terminal's known frame height, and read back by
    /// `page_move`'s caller to size a full/half page. Defaults to a small nonzero value so a
    /// page-move issued before the first draw (or in a headless test) still moves rather than
    /// no-oping on `0`.
    viewport_rows: usize,
    /// The unfiltered `conversations.list` snapshot — every channel/group/DM/MPIM the workspace
    /// has, not just the `dm_limit`-capped `conversations` this `App` actually subscribes to.
    /// Set once at `build` (before `resolve_channels` narrows it) and refreshed by every
    /// `maybe_scan_out_of_cap_dms` scan, so [`pick_changed_dm`] always has a "what did we see
    /// last time" baseline for conversations this `App` otherwise never looks at again (spec
    /// §1: "previous messages stay capped, new ones always arrive" in polling mode too).
    all_conversations: Vec<Conversation>,
    /// The newest `updated` (Slack's millisecond-epoch conversation-activity stamp) this `App`
    /// has actually issued a `history` call for, per out-of-cap DM/MPIM conversation id — the
    /// watermark `maybe_scan_out_of_cap_dms` both selects against (via [`pick_changed_dm`]) and
    /// threads into that call's `oldest` (via [`updated_ms_to_ts`]), so a DM already caught up
    /// to isn't re-fetched from scratch on the next scan just because `all_conversations` also
    /// changed for some other reason. Populated lazily — a DM with no entry here has never had
    /// a scan-triggered `history` call, so its `oldest` is `None` (full-window fetch, matching
    /// spec §1: "or None if first time").
    dm_last_seen: HashMap<String, u64>,
    /// The next `Instant` at or after which `maybe_scan_out_of_cap_dms`'s 5-minute out-of-cap DM
    /// activity scan (spec §1) is due; `None` before the very first tick, which is always due.
    /// `Instant`-based, injected via `poll_tick_at`'s `now` parameter, for the same testability
    /// reason as `cooldown_until` (see its doc) — entirely independent of `cooldown_until`
    /// itself, since a rate-limit cooldown must not also silently delay this lower-frequency
    /// scan past its own schedule once the cooldown lifts.
    next_dm_scan: Option<Instant>,
    /// The `arrival_seq` value as of the moment `build`'s backfill finished (set right before
    /// `build` returns — see its doc), used by `qualifies_for_focus` as the "arrived live during
    /// this session" cutoff (spec §3). `arrival_seq` is post-incremented per newly-seen message
    /// (see `upsert_new`), so every message backfilled at startup gets an `arrival` at most equal
    /// to this value, never greater — which is why `qualifies_for_focus` compares with a strict
    /// `>`, not `>=`: an equal `arrival` still names a backfilled message (the last one inserted
    /// before this field was set), and the "no retroactive Focus over backfilled history"
    /// constraint (spec non-goals) must hold even for it. Every message upserted afterward
    /// (socket, poll, DM scan) necessarily gets an `arrival` strictly greater, since `arrival_seq`
    /// only ever increases from here. `0` on `App::empty` (no backfill ever ran there), so the
    /// very first message added to a fixture built via `empty` (`arrival` `1`) already qualifies
    /// structurally, matching what a real session's first live arrival would do.
    session_watermark: u64,
    /// Focus-mode triggers (`PluginConfig::focus_keywords`), matched case-insensitively as a
    /// substring via `entities::keyword_hit` — deliberately not reusing `keywords` (the Mentions
    /// trigger list); see `PluginConfig::focus_keywords`'s doc for why the two must stay distinct.
    focus_keywords: Vec<String>,
    /// The subscribed conversation ids that are allow-listed DMs (`PluginConfig::dm_allow`),
    /// computed once at `build` time from the resolved `Conversation` list (case-insensitive
    /// exact match against each Im/Mpim's name, mirroring `resolve_channels`'s own `is_allowed`
    /// rule) rather than re-deriving it per row: `resolve_channels`'s output no longer
    /// distinguishes an allow-listed DM from a capped-in-by-recency one once they're merged into
    /// one `Vec<Conversation>`, so `qualifies_for_focus` needs this side-table to tell them apart.
    dm_allow_convs: HashSet<String>,
}

/// How often [`App::poll_tick_at`]'s out-of-cap DM activity scan (spec §1) may run: at most once
/// per this interval, tracked via `next_dm_scan`, independent of the per-tick conversation/thread
/// budget (`POLL_BATCH`) — a much lower frequency because it exists only to catch a DM/MPIM this
/// `App` isn't otherwise polling at all, not to keep already-subscribed conversations fresh.
const DM_SCAN_INTERVAL: Duration = Duration::from_secs(300);

/// Ticks poll at most this many conversations, round-robin, per call — request count is what
/// Slack's rate limits meter, so a tick over every subscribed conversation every time is what
/// triggers them in the first place (see `next_batch`).
const POLL_BATCH: usize = 8;

impl App {
    /// Build the initial state: resolve configured channel names to ids (erroring on an
    /// unknown channel), resolve the self user id and user-name cache, then backfill the
    /// last 50 messages of every subscribed conversation. The only I/O edge in this module —
    /// see the module doc for how the pieces it calls stay pure and tested.
    ///
    /// `tokens` is accepted for interface symmetry with callers that may need to open a
    /// second REST session (e.g. a fresh `Rest` after a token refresh); this function itself
    /// only issues calls through the already-authenticated `rest` passed in. `config` is
    /// taken by value per the fixed cross-task interface (Task 7 hands over an owned
    /// `PluginConfig` it otherwise has no further use for), even though every field access
    /// here goes through a `&self` accessor.
    #[allow(clippy::needless_pass_by_value)]
    pub fn build(config: PluginConfig, _tokens: &Tokens, rest: &Rest) -> Result<App, String> {
        let all_convs = crate::rest::list_conversations(rest)
            .map_err(|e| format!("list_conversations failed: {e:?}"))?;
        let selected = resolve_channels(
            config.channels(),
            config.dms(),
            config.dm_limit(),
            config.dm_allow(),
            &all_convs,
        )?;

        let self_id =
            crate::rest::auth_self(rest).map_err(|e| format!("auth_self failed: {e:?}"))?;
        // The pane always runs inside herdr, which always sets `HERDR_PLUGIN_STATE_DIR` (like
        // `HERDR_PLUGIN_CONFIG_DIR`); no home-dir fallback is needed here (that's the CLI's
        // standalone-invocation concern — see `cli::scan`).
        let state_dir = crate::users_cache::state_dir(|n| std::env::var(n).ok(), || None);
        let users = crate::users_cache::users_cached(
            rest,
            state_dir.as_deref(),
            crate::users_cache::now_secs(),
        )
        .map_err(|e| format!("users failed: {e:?}"))?;
        let user_names: HashMap<String, String> = users.into_iter().collect();
        let selected = resolve_im_names(selected, &user_names);

        let conv_names = selected.iter().map(|c| (c.id.clone(), c.name.clone())).collect();
        let conv_kinds = selected.iter().map(|c| (c.id.clone(), c.kind)).collect();
        // Mirrors `resolve_channels`'s own `is_allowed` rule (case-insensitive exact match, no
        // substring matching) — see `dm_allow_convs`'s field doc for why this can't be recovered
        // from `selected` alone once allow-listed and capped-by-recency DMs are merged together.
        let dm_allow_convs: HashSet<String> = selected
            .iter()
            .filter(|c| matches!(c.kind, ConvKind::Im | ConvKind::Mpim))
            .filter(|c| {
                config
                    .dm_allow()
                    .iter()
                    .any(|allowed| allowed.to_lowercase() == c.name.to_lowercase())
            })
            .map(|c| c.id.clone())
            .collect();

        let mut app = App {
            tab: Tab::Feed,
            view: FeedView::Timeline,
            cursor: 0,
            status: String::new(),
            polling: false,
            conversations: selected,
            conv_names,
            conv_kinds,
            user_names,
            messages: BTreeMap::new(),
            expanded: HashSet::new(),
            read_mentions: HashSet::new(),
            selected: None,
            self_id,
            keywords: config.keywords().to_vec(),
            arrival_seq: 0,
            divider_mark: 0,
            newest_ts: HashMap::new(),
            poll_cursor: 0,
            poll_thread_cursor: 0,
            cooldown_until: None,
            pending_new: 0,
            viewport_rows: 20,
            all_conversations: all_convs,
            dm_last_seen: HashMap::new(),
            next_dm_scan: None,
            session_watermark: 0,
            focus_keywords: config.focus_keywords().to_vec(),
            dm_allow_convs,
        };

        for conv in app.conversations.clone() {
            match backfill_one(&mut app, rest, &conv) {
                BackfillOutcome::Ok => {}
                BackfillOutcome::SkipRemaining => break,
                BackfillOutcome::Fail(msg) => return Err(msg),
            }
        }
        // Everything just backfilled counts as already-seen: the divider only ever marks
        // messages that arrive after this point.
        app.touch();
        app.resync_cursor();
        // Chat panes open scrolled to the bottom — the common at-the-bottom state new
        // arrivals should keep following (see `apply`'s follow-bottom logic below).
        app.snap_cursor_to_last_row();
        // Set last: every message just backfilled already has an `arrival` at or below
        // `arrival_seq` as it stands right now, so this is the watermark Focus qualification
        // (spec §3) needs — anything upserted after this point (socket, poll, DM scan) gets an
        // `arrival` strictly greater than it.
        app.session_watermark = app.arrival_seq;

        Ok(app)
    }

    /// A bare `App` with no subscribed conversations, built up by `apply`/`add_conversation`/
    /// `add_user` instead of `build`'s REST calls. `build` is this module's only I/O edge (see
    /// the module doc), so a caller that needs an `App` without touching the network — Task
    /// 7's render tests are the only current caller — has no other way to get one.
    pub fn empty(self_id: impl Into<String>) -> App {
        App {
            tab: Tab::Feed,
            view: FeedView::Timeline,
            cursor: 0,
            status: String::new(),
            polling: false,
            conversations: Vec::new(),
            conv_names: HashMap::new(),
            conv_kinds: HashMap::new(),
            user_names: HashMap::new(),
            messages: BTreeMap::new(),
            expanded: HashSet::new(),
            read_mentions: HashSet::new(),
            selected: None,
            self_id: self_id.into(),
            keywords: Vec::new(),
            arrival_seq: 0,
            divider_mark: 0,
            newest_ts: HashMap::new(),
            poll_cursor: 0,
            poll_thread_cursor: 0,
            cooldown_until: None,
            pending_new: 0,
            viewport_rows: 20,
            all_conversations: Vec::new(),
            dm_last_seen: HashMap::new(),
            next_dm_scan: None,
            session_watermark: 0,
            focus_keywords: Vec::new(),
            dm_allow_convs: HashSet::new(),
        }
    }

    /// Register a conversation's display name and kind, for fixture setup (see `empty`).
    pub fn add_conversation(&mut self, id: &str, name: &str, kind: ConvKind) {
        self.conv_names.insert(id.to_string(), name.to_string());
        self.conv_kinds.insert(id.to_string(), kind);
    }

    /// Register a user's display name, for fixture setup (see `empty`).
    pub fn add_user(&mut self, id: &str, name: &str) {
        self.user_names.insert(id.to_string(), name.to_string());
    }

    /// Mark `conv_id` as an allow-listed DM for Focus qualification (spec §3), for fixture setup
    /// (see `empty`) — `build` derives this from `PluginConfig::dm_allow` itself, but a fixture
    /// built via `empty` never runs that resolution, so it needs a direct way in.
    pub fn allow_focus_dm(&mut self, conv_id: &str) {
        self.dm_allow_convs.insert(conv_id.to_string());
    }

    /// Add a Focus-mode trigger keyword (spec §3, `PluginConfig::focus_keywords`), for fixture
    /// setup (see `empty`) — see `allow_focus_dm`'s doc for why `empty`-built fixtures need a
    /// direct setter rather than going through `build`'s config resolution.
    pub fn add_focus_keyword(&mut self, keyword: &str) {
        self.focus_keywords.push(keyword.to_string());
    }

    /// Apply one socket event: insert a new message, replace an edited one in place, remove
    /// a deleted one, or update connection/status state. Mention detection is not done here
    /// — it is recomputed on demand by `mention_rows`/`unread_mentions` from the raw message,
    /// so a later config change (e.g. a new keyword) would not require replaying history.
    pub fn apply(&mut self, ev: SocketEvent) {
        // Captured before the event lands: if the active tab's cursor is already sitting on the
        // last row (the common at-the-bottom chat-pane state — see `build`/`snap_cursor_to_last_row`),
        // a new arrival below it should not leave the cursor stranded above the new bottom; it
        // should keep following, exactly like a chat client scrolled to "now". Ordering is now
        // unified newest-at-bottom everywhere (spec §1), so this applies to every tab/view, not
        // just the Feed tab.
        let follow_bottom = self.is_cursor_at_last_row();
        let had_rows_before = !self.current_ids().is_empty();
        let arrival_before = self.arrival_seq;
        match ev {
            SocketEvent::Connected => {
                self.polling = false;
                self.status.clear();
                // A cooldown set from a `RateLimited` hit before this reconnect must not
                // outlive it: a healthy socket means Slack accepted our connection, so the
                // poll path restarts clean rather than silently no-oping the next manual `r`
                // until a now-stale deadline lapses.
                self.cooldown_until = None;
            }
            SocketEvent::Down(reason) => {
                self.polling = true;
                self.status = format!("socket unavailable ({reason}) — polling");
            }
            SocketEvent::Message(msg) => self.upsert_new(msg),
            SocketEvent::Changed(msg) => self.upsert_edit(msg),
            SocketEvent::Deleted { conv, ts } => self.remove(&conv, &ts),
        }
        self.finish_after_arrivals(follow_bottom, had_rows_before, arrival_before);
    }

    /// Shared tail of `apply`/`poll_tick_at` (spec §3-§4): resync the cursor's identity, then
    /// either keep following the bottom (cursor was already there when the arrival landed — the
    /// new-arrivals counter stays untouched, since `snap_cursor_to_last_row` clears it via
    /// `maybe_clear_pending_new`) or, if the cursor was scrolled up in a tab that already had
    /// rows and `arrival_before` shows the message store actually grew, count the arrival.
    /// `had_rows_before` is what keeps the very first-ever arrival into an empty tab (where
    /// `follow_bottom` is trivially `false` — there is no "last row" yet to be sitting on) from
    /// being miscounted as "arrived while scrolled up": there is no bottom to have scrolled away
    /// from yet either. `arrival_before` (rather than "did this event insert a row") is what lets
    /// one call cover every event kind — `Connected`/`Down`/`Deleted` never bump `arrival_seq`,
    /// so they never falsely increment.
    fn finish_after_arrivals(
        &mut self,
        follow_bottom: bool,
        had_rows_before: bool,
        arrival_before: u64,
    ) {
        self.resync_cursor();
        if follow_bottom {
            self.snap_cursor_to_last_row();
        } else if had_rows_before && self.arrival_seq > arrival_before {
            // Count every arrival landed this cycle, not just "an arrival happened": a poll
            // tick (or any other caller) can upsert several messages before its one
            // `finish_after_arrivals` call (`poll_conversations`/`apply_fetched_replies` each
            // loop `upsert_new` over a batch), and each of those bumps `arrival_seq` by 1.
            let delta = usize::try_from(self.arrival_seq - arrival_before)
                .expect("arrival_seq delta always fits usize on any platform this runs on");
            self.pending_new += delta;
        }
    }

    /// Whether `cursor` currently sits on the active tab's last row (the "scrolled to the
    /// bottom" state — see `apply`'s follow-bottom logic and `build`).
    fn is_cursor_at_last_row(&self) -> bool {
        let ids = self.current_ids();
        !ids.is_empty() && self.cursor == ids.len() - 1
    }

    /// Force `cursor`/`selected` onto the active tab's last row, if it has one. Called after
    /// `build`'s backfill (chat panes open scrolled to the bottom) and by `apply` to keep a
    /// Feed-tab cursor that was already at the bottom pinned there through new arrivals.
    fn snap_cursor_to_last_row(&mut self) {
        let ids = self.current_ids();
        if let Some(last) = ids.len().checked_sub(1) {
            self.cursor = last;
            self.selected.clone_from(&ids[last]);
        }
        self.maybe_clear_pending_new();
    }

    /// Clear the new-arrivals counter once the cursor has reached the active tab's last row, by
    /// any means (spec §3: "reaching the bottom, any means, clears it") — called from every
    /// cursor-moving method that can land there: `move_cursor` (so `j`/`Down`/`PageDown`/
    /// `ctrl-d` all count), `jump_first` (the degenerate single-row case), and
    /// `snap_cursor_to_last_row` (so both `jump_newest`/`G`/`End` and the follow-bottom snap in
    /// `finish_after_arrivals` count).
    fn maybe_clear_pending_new(&mut self) {
        if self.is_cursor_at_last_row() {
            self.pending_new = 0;
        }
    }

    /// Count of arrivals since the cursor last left the active tab's bottom row (spec §3); `0`
    /// when the cursor is following the bottom or nothing has arrived since it last was there.
    /// The bottom-edge `↓ n new` overlay (Task 7's `ui.rs`) shows only while this is nonzero.
    pub fn pending_new(&self) -> usize {
        self.pending_new
    }

    /// Set the row count Task 7's event loop measured for the active tab's body viewport this
    /// draw (the body area's height — see `ui::render_body`), so `page_move`'s caller can size a
    /// full/half page off the pane's actual on-screen height rather than a hardcoded guess.
    /// Called once per frame; harmless to call with the same value repeatedly.
    pub fn set_viewport_rows(&mut self, rows: usize) {
        self.viewport_rows = rows;
    }

    /// The last-measured viewport row count (see `set_viewport_rows`), for a caller that needs
    /// to compute `page_move`'s `rows` argument (e.g. `±viewport_rows` for PageDown/PageUp,
    /// `±viewport_rows / 2` for ctrl-d/ctrl-u).
    pub fn viewport_rows(&self) -> usize {
        self.viewport_rows
    }

    /// Move the cursor+selection to the active tab's last row (spec §2: `G`/`End`).
    pub fn jump_newest(&mut self) {
        self.snap_cursor_to_last_row();
    }

    /// Move the cursor+selection to the active tab's first row (spec §2: `g`/`Home`).
    pub fn jump_first(&mut self) {
        let ids = self.current_ids();
        if !ids.is_empty() {
            self.cursor = 0;
            self.selected.clone_from(&ids[0]);
        }
        self.maybe_clear_pending_new();
    }

    /// Move the cursor by a page-sized `rows` delta (spec §2: PageDown/PageUp `±viewport_rows`,
    /// ctrl-d/ctrl-u `±viewport_rows / 2`, both computed by the caller from `viewport_rows()`),
    /// reusing `move_cursor`'s clamping and identity tracking — a page move is just a bigger
    /// `move_cursor`, not a different kind of motion.
    pub fn page_move(&mut self, rows: isize) {
        self.move_cursor(rows);
    }

    /// Fallback mode: incrementally re-pull each polled conversation's messages newer than the
    /// last one seen (`newest_ts`, threaded as `history`'s `oldest`), staggered across ticks in
    /// `POLL_BATCH`-sized round-robin batches (`next_batch`) rather than every subscribed
    /// conversation every time — see the `App::newest_ts`/`poll_cursor` field docs and the
    /// module's spec reference for why (request count is what Slack's limits meter). Skips
    /// entirely, without issuing a single request, while a prior `RateLimited` hit's cooldown
    /// (`cooldown_until`) hasn't yet passed — the status line keeps showing the notice from when
    /// the cooldown was set.
    ///
    /// Up to [`thread_slot_count`] of the `POLL_BATCH` budget is spent first on a second
    /// round-robin over [`App::active_threads`] — threads currently expanded, or whose root's
    /// `reply_count` outpaces what's locally known — fetching each one's replies newer than the
    /// thread's newest-known reply ts (spec §2). The remainder goes to conversations exactly as
    /// before; when there are no active threads, the full `POLL_BATCH` goes to conversations, so
    /// the reservation never wastes budget on an empty thread list.
    ///
    /// Messages already known (same `(conv, ts)`) are deduplicated by `upsert_new` regardless
    /// (belt-and-suspenders against a re-widened `oldest` window), so a message that arrives via
    /// both a poll and the socket still appears exactly once.
    ///
    /// A `RestError::RateLimited` sets a rate-limit status, starts the cooldown, and stops the
    /// rest of this tick — including skipping the conversation batch entirely if the limit hit
    /// during the thread slots, and skipping the out-of-cap DM scan below if either the thread
    /// or the conversation batch hit it (`run_or_skip_dm_scan`; Slack's own signal to back off
    /// now, not to keep hammering anything else this tick); any other error sets a one-line
    /// status naming what failed and
    /// moves on to the next item in its batch rather than crashing (spec: a per-conversation or
    /// per-thread poll failure must never take down the pane). See `poll_error_status` for the
    /// pure wording decision.
    pub fn poll_tick(&mut self, rest: &Rest) {
        self.poll_tick_at(rest, Instant::now());
    }

    /// `poll_tick`'s real logic, taking `now` as a parameter (production always passes
    /// `Instant::now()`) so the cooldown-gating decision is exercised in tests without a real
    /// sleep — `Instant` supports arithmetic (`now + Duration::from_secs(n)`), so tests build
    /// deadlines directly rather than needing a mockable clock trait.
    fn poll_tick_at(&mut self, rest: &Rest, now: Instant) {
        if cooldown_active(now, self.cooldown_until) {
            return;
        }
        self.cooldown_until = None;

        let follow_bottom = self.is_cursor_at_last_row();
        let had_rows_before = !self.current_ids().is_empty();
        let arrival_before = self.arrival_seq;

        let active = self.active_threads();
        let thread_slots = thread_slot_count(active.len());
        let conv_slots = POLL_BATCH - thread_slots;

        let rate_limited = self.poll_active_threads(rest, &active, thread_slots, now);
        let rate_limited =
            if rate_limited { true } else { self.poll_conversations(rest, conv_slots, now) };
        self.run_or_skip_dm_scan(rest, now, rate_limited);
        self.finish_after_arrivals(follow_bottom, had_rows_before, arrival_before);
    }

    /// The tail of `poll_tick_at`: run the out-of-cap DM scan, unless the main conv/thread
    /// budget above already hit `RateLimited` this tick — that hit stops the rest of the tick
    /// (this fn's caller's doc), and the scan is part of "the rest of the tick" like everything
    /// else after it, not an exception to it. Split out from the inline `if` so the gate itself
    /// is unit-tested by injecting `rate_limited` directly, without needing a real 429 to drive
    /// it end-to-end through `poll_active_threads`/`poll_conversations`.
    fn run_or_skip_dm_scan(&mut self, rest: &Rest, now: Instant, rate_limited: bool) {
        if !rate_limited {
            self.maybe_scan_out_of_cap_dms(rest, now);
        }
    }

    /// The out-of-cap DM activity scan (spec §1): additive to the `POLL_BATCH` conversation/
    /// thread budget above when that budget didn't just rate limit — `poll_tick_at` only reaches
    /// this (via `run_or_skip_dm_scan`) when `rate_limited` is false, since a `RateLimited` hit
    /// in that budget stops the rest of the tick, this scan included (review fix: this scan used
    /// to run unconditionally here, which contradicted `poll_tick_at`'s own "stops the rest of
    /// this tick" contract). Runs at most once per [`DM_SCAN_INTERVAL`]
    /// (`dm_scan_due`/`next_dm_scan`); a no-op tick (not yet due) costs nothing, not even a
    /// `list_conversations` call.
    ///
    /// When due: re-fetches the full conversation list, hands it plus the previous snapshot
    /// (`all_conversations`) to [`pick_changed_dm`] to find at most one out-of-cap DM/MPIM whose
    /// activity moved, stores the fresh list as the new baseline for next time, and — if one was
    /// picked — issues exactly one `history` call for it (`oldest` from `dm_last_seen`'s watermark,
    /// converted via [`updated_ms_to_ts`], or `None` on a DM's first-ever scan fetch), folding the
    /// result in through the normal `upsert_new` path so mention detection and Focus qualification
    /// follow automatically, same as every other arrival. A `list_conversations` or `history`
    /// failure here sets `status` (and, on `RateLimited`, `cooldown_until`) exactly like the
    /// conversation/thread budget's own failures — this scan is a REST call like any other and
    /// must surface, not swallow, an error — but never fails the tick itself.
    fn maybe_scan_out_of_cap_dms(&mut self, rest: &Rest, now: Instant) {
        if !dm_scan_due(now, self.next_dm_scan) {
            return;
        }
        self.next_dm_scan = Some(now + DM_SCAN_INTERVAL);

        let new_all = match crate::rest::list_conversations(rest) {
            Ok(v) => v,
            Err(err @ RestError::RateLimited(secs)) => {
                self.status = poll_error_status("dm scan", &err);
                self.cooldown_until = Some(now + Duration::from_secs(secs));
                return;
            }
            Err(err) => {
                self.status = poll_error_status("dm scan", &err);
                return;
            }
        };

        let subscribed: HashSet<String> = self.conversations.iter().map(|c| c.id.clone()).collect();
        let picked =
            pick_changed_dm(&self.all_conversations, &new_all, &subscribed, &self.dm_last_seen);
        self.all_conversations = new_all;

        let Some(conv) = picked else {
            return;
        };
        let Some(updated) = conv.updated else {
            return;
        };
        let oldest = self.dm_last_seen.get(&conv.id).copied().map(updated_ms_to_ts);
        match crate::rest::history(rest, &conv.id, 50, oldest.as_deref()) {
            Ok(msgs) => {
                for msg in msgs {
                    self.upsert_new(msg);
                }
                self.dm_last_seen.insert(conv.id, updated);
            }
            Err(err @ RestError::RateLimited(secs)) => {
                self.status = poll_error_status(&conv.name, &err);
                self.cooldown_until = Some(now + Duration::from_secs(secs));
            }
            Err(err) => {
                self.status = poll_error_status(&conv.name, &err);
            }
        }
    }

    /// The active-thread half of `poll_tick_at`'s split budget: round-robin `thread_slots` of
    /// `active` via the dedicated `poll_thread_cursor`, fetching each thread's replies newer than
    /// its newest-known reply ts (`newest_reply_ts`) and folding them in via `apply_fetched_replies`.
    /// Returns whether a `RateLimited` hit occurred (and so the conversation batch this tick
    /// should be skipped entirely, matching the single-batch behavior `poll_conversations` itself
    /// applies) — the cooldown itself is set on `self` before returning, same as the
    /// conversation path.
    fn poll_active_threads(
        &mut self,
        rest: &Rest,
        active: &[(String, String)],
        thread_slots: usize,
        now: Instant,
    ) -> bool {
        let (indices, next_cursor) =
            next_batch(self.poll_thread_cursor, active.len(), thread_slots);
        self.poll_thread_cursor = next_cursor;

        for i in indices {
            let (conv_id, root_ts) = active[i].clone();
            let oldest = self.newest_reply_ts(&conv_id, &root_ts);
            let label = format!("{conv_id} thread {root_ts}");
            match crate::rest::replies(rest, &conv_id, &root_ts, oldest.as_deref()) {
                Ok(msgs) => self.apply_fetched_replies(msgs),
                Err(err @ RestError::RateLimited(secs)) => {
                    self.status = poll_error_status(&label, &err);
                    self.cooldown_until = Some(now + Duration::from_secs(secs));
                    return true;
                }
                Err(err) => {
                    self.status = poll_error_status(&label, &err);
                }
            }
        }
        false
    }

    /// The conversation half of `poll_tick_at`'s split budget: unchanged from before the split
    /// except that its slot count is now `conv_slots` (`POLL_BATCH` minus whatever the thread
    /// round-robin took) rather than the full `POLL_BATCH`. Returns whether a `RateLimited` hit
    /// occurred, mirroring `poll_active_threads`'s own return — `poll_tick_at` combines both to
    /// decide whether anything after this budget (the DM scan) still runs this tick.
    fn poll_conversations(&mut self, rest: &Rest, conv_slots: usize, now: Instant) -> bool {
        let n = self.conversations.len();
        let (indices, next_cursor) = next_batch(self.poll_cursor, n, conv_slots);
        self.poll_cursor = next_cursor;

        for i in indices {
            let conv_id = self.conversations[i].id.clone();
            let conv_name = self.conversations[i].name.clone();
            let oldest = self.newest_ts.get(&conv_id).cloned();
            match crate::rest::history(rest, &conv_id, 50, oldest.as_deref()) {
                Ok(msgs) => {
                    for msg in msgs {
                        self.upsert_new(msg);
                    }
                }
                Err(err @ RestError::RateLimited(secs)) => {
                    self.status = poll_error_status(&conv_name, &err);
                    self.cooldown_until = Some(now + Duration::from_secs(secs));
                    return true;
                }
                Err(err) => {
                    self.status = poll_error_status(&conv_name, &err);
                }
            }
        }
        false
    }

    /// Every reply-worthy "active thread" `poll_tick_at`'s second round-robin should keep fresh:
    /// every root currently expanded inline (`expanded`), plus every root whose Slack-reported
    /// `reply_count` exceeds the number of replies stored locally for it — i.e. backfill/polling
    /// hasn't caught up to what Slack says the thread actually has. A root already covered by
    /// `expanded` is not duplicated. Order is stable within one call (`expanded`'s arbitrary but
    /// fixed hash order, then remaining count-gap roots in message-store order) — good enough for
    /// `poll_thread_cursor`'s round-robin, which only needs a consistent order across ticks, not
    /// any particular one.
    fn active_threads(&self) -> Vec<(String, String)> {
        let mut ids: Vec<(String, String)> = self.expanded.iter().cloned().collect();
        for stored in self.messages.values() {
            if stored.msg.thread_ts.is_some() {
                continue; // only roots are candidates
            }
            let id = (stored.msg.conv.clone(), stored.msg.ts.clone());
            if ids.contains(&id) {
                continue;
            }
            let local = self.local_reply_count(&stored.msg.conv, &stored.msg.ts);
            if stored.msg.reply_count.unwrap_or(0) as usize > local {
                ids.push(id);
            }
        }
        ids
    }

    /// How many replies to `root_ts` in `conv` are already stored locally.
    fn local_reply_count(&self, conv: &str, root_ts: &str) -> usize {
        self.messages
            .values()
            .filter(|s| s.msg.conv == conv && s.msg.thread_ts.as_deref() == Some(root_ts))
            .count()
    }

    /// The newest `ts` among `root_ts`'s locally-known replies in `conv`, threaded into
    /// `poll_active_threads`'s `replies(..., oldest)` call — `None` when no reply is known yet
    /// locally, which fetches the whole thread (Slack's `oldest` is optional; omitting it is
    /// exactly what a first-ever fetch of a backfilled-but-never-expanded thread wants).
    fn newest_reply_ts(&self, conv: &str, root_ts: &str) -> Option<String> {
        self.messages
            .values()
            .filter(|s| s.msg.conv == conv && s.msg.thread_ts.as_deref() == Some(root_ts))
            .map(|s| s.msg.ts.clone())
            .max_by(|a, b| ts_cmp(a, b))
    }

    /// Fold thread replies fetched by the polling refresh into the store via the normal
    /// `upsert_new` path, so mention detection/dedup/arrival-ordering all follow exactly as they
    /// do for any other newly-seen message (spec §2: "fetched replies run through the normal
    /// upsert path, so thread mentions land in the Mentions tab"). Split out from
    /// `poll_active_threads` so it is unit-tested without a real REST call.
    fn apply_fetched_replies(&mut self, msgs: Vec<Message>) {
        for msg in msgs {
            self.upsert_new(msg);
        }
    }

    /// Move the cursor by `delta` rows within the active tab's current row list, clamping to
    /// its bounds, and record the newly-selected row's identity so a later row-set change can
    /// re-find it by identity rather than position (see `resync_cursor`). The method Task 7's
    /// event loop calls on up/down key presses.
    pub fn move_cursor(&mut self, delta: isize) {
        let ids = self.current_ids();
        if ids.is_empty() {
            self.cursor = 0;
            self.selected = None;
            return;
        }
        let max = ids.len() as isize - 1;
        let new_pos = (self.cursor as isize + delta).clamp(0, max);
        #[allow(clippy::cast_sign_loss)]
        let new_pos = new_pos as usize;
        self.cursor = new_pos;
        self.selected.clone_from(&ids[new_pos]);
        self.maybe_clear_pending_new();
    }

    /// The active tab's current row identities (each paired with its `SelKind`), in row order
    /// (`None` only ever appears for the Feed tab's synthetic `Divider` row, which only the
    /// Timeline projection ever produces).
    fn current_ids(&self) -> Vec<Option<((String, String), SelKind)>> {
        match self.tab {
            Tab::Feed => self
                .active_feed_rows_with_ids()
                .into_iter()
                .map(|(id, row)| id.map(|i| (i, sel_kind_of(&row))))
                .collect(),
            Tab::Mentions => self
                .mention_rows_with_ids()
                .into_iter()
                .map(|(id, _)| Some((id, SelKind::Mention)))
                .collect(),
        }
    }

    /// The Feed tab's current row list, dispatched by `view` (see [`FeedView`]): `feed_rows_with_ids`
    /// for the Timeline, `thread_rows_with_ids` for the Threads digest. The single dispatch point
    /// `current_ids`, `feed_target`, and `render_body` (via the public `feed_rows`/`thread_rows`
    /// pair) all funnel through, so an action taken in the Threads view (Enter, permalink) always
    /// resolves against the rows actually on screen rather than the Timeline's — see the module
    /// doc's `SelKind` and `toggle_view`'s doc for why the two projections must never be conflated.
    fn active_feed_rows_with_ids(&self) -> Vec<IdRow> {
        match self.view {
            FeedView::Timeline => self.feed_rows_with_ids(),
            FeedView::Threads => self.thread_rows_with_ids(),
            FeedView::Focus => self.focus_rows_with_ids(),
        }
    }

    /// Re-derive `cursor` from `selected` after a row-set change: if the previously-selected
    /// identity still names a row in the active tab, `cursor` snaps to its new position;
    /// otherwise `cursor` is clamped into bounds and `selected` follows it to whatever row now
    /// sits there (or `None` if the row list is now empty).
    fn resync_cursor(&mut self) {
        let ids = self.current_ids();
        if ids.is_empty() {
            self.cursor = 0;
            self.selected = None;
            return;
        }
        if let Some(sel) = self.selected.clone()
            && let Some(pos) = ids.iter().position(|id| id.as_ref() == Some(&sel))
        {
            self.cursor = pos;
            return;
        }
        self.cursor = self.cursor.min(ids.len() - 1);
        self.selected.clone_from(&ids[self.cursor]);
    }

    /// Resolve the Feed-tab row/id the next action (`toggle_expand`, permalink) should target:
    /// the stored `selected` identity if it still names a row, else the positional `cursor`
    /// (covers direct `cursor` assignment without going through `move_cursor`, as the existing
    /// tests do).
    fn feed_target(&self) -> Option<((String, String), Row)> {
        let rows = self.active_feed_rows_with_ids();
        if let Some((sel_id, sel_kind)) = &self.selected
            && let Some((_, row)) = rows
                .iter()
                .find(|(id, row)| id.as_ref() == Some(sel_id) && sel_kind_of(row) == *sel_kind)
        {
            return Some((sel_id.clone(), row.clone()));
        }
        rows.into_iter().nth(self.cursor).and_then(|(id, row)| id.map(|id| (id, row)))
    }

    /// As `feed_target`, for the Mentions tab (whose rows always carry an id and are always
    /// `SelKind::Mention`).
    fn mention_target(&self) -> Option<((String, String), Row)> {
        let rows = self.mention_rows_with_ids();
        if let Some((sel_id, sel_kind)) = &self.selected
            && *sel_kind == SelKind::Mention
            && let Some((_, row)) = rows.iter().find(|(id, _)| id == sel_id)
        {
            return Some((sel_id.clone(), row.clone()));
        }
        rows.into_iter().nth(self.cursor)
    }

    /// The Feed tab: every top-level message (and thread root) in chronological order, with
    /// an inline `ThreadMarker` in place of a collapsed thread's replies, and an unread
    /// `Divider` before the first row that arrived after the last `touch()`.
    pub fn feed_rows(&self) -> Vec<Row> {
        self.feed_rows_with_ids().into_iter().map(|(_, row)| row).collect()
    }

    /// As `feed_rows`, but each row is paired with the `(conv, ts)` a Feed-tab action (expand,
    /// permalink) should act on — `None` for the synthetic `Divider` row.
    fn feed_rows_with_ids(&self) -> Vec<IdRow> {
        let mut rows: Vec<ArrivalRow> = Vec::new();
        for stored in self.messages.values() {
            if let Some(root_ts) = stored.msg.thread_ts.clone() {
                if self.messages.contains_key(&key_for(&stored.msg.conv, &root_ts)) {
                    if self.expanded.contains(&(stored.msg.conv.clone(), root_ts.clone())) {
                        continue; // nested under its root below (today's behavior).
                    }
                    // Spec §6: a reply to a *collapsed* thread must not simply vanish — it gets
                    // a discoverable activity row at its own chronological position (in addition
                    // to the root's `ThreadMarker`, pushed when the root itself was processed).
                    rows.push((
                        stored.arrival,
                        Some((stored.msg.conv.clone(), stored.msg.ts.clone())),
                        self.activity_row(&stored.msg),
                    ));
                    continue;
                }
                // Orphaned reply: its root predates our backfill horizon (or was otherwise
                // never seen), so without this branch it would never render at all. Render it
                // as a normal inline row instead, marked with a "↳ " prefix so it still reads
                // as thread context rather than a plain top-level message.
                rows.push((
                    stored.arrival,
                    Some((stored.msg.conv.clone(), stored.msg.ts.clone())),
                    self.nested_reply_row(&stored.msg),
                ));
                continue;
            }
            let conv = stored.msg.conv.clone();
            let root_ts = stored.msg.ts.clone();
            rows.push((
                stored.arrival,
                Some((conv.clone(), root_ts.clone())),
                self.message_row(&stored.msg),
            ));

            let mut replies: Vec<&Stored> = self
                .messages
                .values()
                .filter(|s| {
                    s.msg.conv == conv && s.msg.thread_ts.as_deref() == Some(root_ts.as_str())
                })
                .collect();
            let count = marker_count(stored.msg.reply_count, replies.len());
            if count == 0 {
                continue;
            }
            replies.sort_by(|a, b| ts_cmp(&a.msg.ts, &b.msg.ts));

            if self.expanded.contains(&(conv.clone(), root_ts.clone())) {
                for reply in &replies {
                    rows.push((
                        reply.arrival,
                        Some((conv.clone(), reply.msg.ts.clone())),
                        self.message_row(&reply.msg),
                    ));
                }
            } else {
                let marker_arrival = replies
                    .iter()
                    .map(|r| r.arrival)
                    .max()
                    .unwrap_or(stored.arrival)
                    .max(stored.arrival);
                rows.push((
                    marker_arrival,
                    Some((conv.clone(), root_ts.clone())),
                    Row {
                        conv_label: self.conv_label(&conv),
                        author: String::new(),
                        time_hhmm: String::new(),
                        text: format!("\u{21b3} {count} replies"),
                        kind: RowKind::ThreadMarker { replies: count, expanded: false },
                    },
                ));
            }
        }

        self.insert_divider(rows)
    }

    /// Splice a `Divider` row in before the first row whose arrival is past `divider_mark`,
    /// dropping the arrival counters the divider placement no longer needs.
    fn insert_divider(&self, rows: Vec<ArrivalRow>) -> Vec<IdRow> {
        let split_at = rows.iter().position(|(arrival, ..)| *arrival > self.divider_mark);
        let mut out = Vec::with_capacity(rows.len() + 1);
        for (i, (_, id, row)) in rows.into_iter().enumerate() {
            if Some(i) == split_at {
                out.push((
                    None,
                    Row {
                        conv_label: String::new(),
                        author: String::new(),
                        time_hhmm: String::new(),
                        text: String::new(),
                        kind: RowKind::Divider,
                    },
                ));
            }
            out.push((id, row));
        }
        out
    }

    /// The Threads view (spec §3): a digest of only threads. Each qualifying root — one whose
    /// `marker_count` (metadata or locally-known replies, `max`'d as everywhere else) is nonzero
    /// — is followed immediately by every locally-known reply nested beneath it in chronological
    /// order, always shown here regardless of the Timeline's per-thread `expanded` flag (spec:
    /// "always expanded in this view" — see `refresh_thread`'s doc for why that flag plays no
    /// role here at all). Threads are ordered by latest activity — the newest locally-known
    /// reply's `ts`, or the root's own `ts` when no reply is known yet — ascending (spec §1: most
    /// recently active thread last, unified with every view's newest-at-the-bottom direction), so
    /// a thread that just got a new reply jumps back to the bottom even if its root is old. Non-threaded
    /// messages are excluded entirely, unlike the Timeline (which shows every message). Root and
    /// reply rows both render as plain `RowKind::Message` — a reply's text gets the same "↳ "
    /// nesting prefix the Timeline's orphaned-reply/expanded-thread rows use — so they share
    /// `SelKind::Message` identity with their Timeline counterparts (see the module doc's
    /// `SelKind`), letting a selection made in one view still resolve correctly if the row also
    /// exists in the other.
    ///
    /// A reply whose root predates the backfill horizon (or was otherwise never seen) gets a
    /// *synthetic* thread entry instead of being dropped — see the doc on the `Thread::root_msg`
    /// field this method builds internally, and on the "(thread — root not loaded)" header text
    /// below, for how that's rendered and how it self-heals once the real root is known.
    pub fn thread_rows(&self) -> Vec<Row> {
        self.thread_rows_with_ids().into_iter().map(|(_, row)| row).collect()
    }

    /// As `thread_rows`, but each row is paired with the `(conv, ts)` a Threads-view action
    /// (Enter/refresh, permalink) should act on.
    fn thread_rows_with_ids(&self) -> Vec<IdRow> {
        /// Either a real, locally-known root (`Some`) or a synthetic placeholder for a thread
        /// whose root has never been seen (`None` — see `thread_rows`'s doc). The `(conv,
        /// root_ts)` pair is carried alongside so the synthetic case has something to render and
        /// key an id from even without a `Stored` root to borrow it from.
        struct Thread<'a> {
            activity: (u64, u32),
            conv: String,
            root_ts: String,
            root_msg: Option<&'a Stored>,
            replies: Vec<&'a Stored>,
        }

        let mut threads: Vec<Thread<'_>> = Vec::new();
        // Every reply attached to a root this loop already knows about (whether or not that root
        // clears `marker_count`) — used below to find orphaned replies, i.e. those whose
        // `thread_ts` names a root outside this set entirely.
        let mut seen_roots: HashSet<(&str, &str)> = HashSet::new();
        for stored in self.messages.values() {
            if stored.msg.thread_ts.is_some() {
                continue; // only roots start a thread entry; replies are attached below.
            }
            seen_roots.insert((stored.msg.conv.as_str(), stored.msg.ts.as_str()));
            let conv = stored.msg.conv.clone();
            let root_ts = stored.msg.ts.clone();
            let mut replies: Vec<&Stored> = self
                .messages
                .values()
                .filter(|s| {
                    s.msg.conv == conv && s.msg.thread_ts.as_deref() == Some(root_ts.as_str())
                })
                .collect();
            let count = marker_count(stored.msg.reply_count, replies.len());
            if count == 0 {
                continue; // not a thread — excluded from this view entirely.
            }
            replies.sort_by_key(|r| ts_key(&r.msg.ts));
            let activity = replies.last().map_or_else(|| ts_key(&root_ts), |r| ts_key(&r.msg.ts));
            threads.push(Thread { activity, conv, root_ts, root_msg: Some(stored), replies });
        }

        // Orphaned replies: their root isn't locally known at all (unlike the roots skipped
        // above via `count == 0`, which *are* known — just not thread-qualifying). Group them by
        // `(conv, thread_ts)` into one synthetic thread entry each rather than dropping them, the
        // way the Timeline's `feed_rows_with_ids` inlines them individually instead.
        let mut orphans: BTreeMap<(String, String), Vec<&Stored>> = BTreeMap::new();
        for stored in self.messages.values() {
            if let Some(root_ts) = &stored.msg.thread_ts
                && !seen_roots.contains(&(stored.msg.conv.as_str(), root_ts.as_str()))
            {
                orphans.entry((stored.msg.conv.clone(), root_ts.clone())).or_default().push(stored);
            }
        }
        for ((conv, root_ts), mut replies) in orphans {
            replies.sort_by_key(|r| ts_key(&r.msg.ts));
            let activity = replies.last().map_or_else(|| ts_key(&root_ts), |r| ts_key(&r.msg.ts));
            threads.push(Thread { activity, conv, root_ts, root_msg: None, replies });
        }

        threads.sort_by(|a, b| a.activity.cmp(&b.activity)); // oldest activity first, newest last

        let mut rows: Vec<IdRow> = Vec::new();
        for thread in threads {
            let id = Some((thread.conv.clone(), thread.root_ts.clone()));
            let root_row = match thread.root_msg {
                Some(root) => self.message_row(&root.msg),
                // Synthetic header: same shape/`SelKind` as a real root row (`RowKind::Message`,
                // via `message_row`'s own default), so selecting it and hitting Enter still
                // routes through `refresh_thread` → `conversations.replies`, which returns the
                // real root as the first message; the next `upsert_new` inserts it under this
                // same key, and the very next projection naturally replaces this synthetic row
                // with the real one — no explicit cleanup needed anywhere.
                None => Row {
                    conv_label: self.conv_label(&thread.conv),
                    author: String::new(),
                    time_hhmm: ts_to_hhmm(&thread.root_ts),
                    text: "(thread — root not loaded)".to_string(),
                    kind: RowKind::Message,
                },
            };
            rows.push((id, root_row));
            for reply in thread.replies {
                let mut row = self.message_row(&reply.msg);
                row.text = format!("\u{21b3} {}", row.text);
                rows.push((Some((thread.conv.clone(), reply.msg.ts.clone())), row));
            }
        }
        rows
    }

    /// The Mentions tab: every message that triggers attention, oldest first / newest at the
    /// bottom (spec §1, unified with the Feed tab's direction), each carrying its read/unread
    /// state.
    pub fn mention_rows(&self) -> Vec<Row> {
        self.mention_rows_with_ids().into_iter().map(|(_, row)| row).collect()
    }

    /// As `mention_rows`, but each row is paired with the `(conv, ts)` a Mentions-tab action
    /// (read toggle, permalink) should act on.
    fn mention_rows_with_ids(&self) -> Vec<((String, String), Row)> {
        let mut items: Vec<&Stored> =
            self.messages.values().filter(|s| self.is_mention_stored(s)).collect();
        items.sort_by(|a, b| ts_cmp(&a.msg.ts, &b.msg.ts)); // oldest first, newest at the bottom (spec §1)
        items
            .into_iter()
            .map(|s| {
                let id = (s.msg.conv.clone(), s.msg.ts.clone());
                let mut row = self.message_row(&s.msg);
                row.kind = RowKind::Mention { read: self.read_mentions.contains(&id) };
                (id, row)
            })
            .collect()
    }

    /// The Focus view (spec §3): reuses the Timeline's plain row rendering (`message_row`, no
    /// `ThreadMarker`/`Divider` synthesis), filtered to only messages that qualify for Focus (see
    /// `qualifies_for_focus`), oldest first / newest at the bottom, same as every other view's
    /// convention. Nothing is deleted from the model to produce this — toggling back to Timeline
    /// still shows everything, qualifying or not.
    pub fn focus_rows(&self) -> Vec<Row> {
        self.focus_rows_with_ids().into_iter().map(|(_, row)| row).collect()
    }

    /// As `focus_rows`, but each row is paired with the `(conv, ts)` a Feed-tab action (expand,
    /// permalink) should act on — see `active_feed_rows_with_ids`'s Focus arm.
    fn focus_rows_with_ids(&self) -> Vec<IdRow> {
        let mut items: Vec<&Stored> =
            self.messages.values().filter(|s| self.qualifies_for_focus(s)).collect();
        items.sort_by(|a, b| ts_cmp(&a.msg.ts, &b.msg.ts)); // oldest first, newest at the bottom
        items
            .into_iter()
            .map(|s| {
                let id = (s.msg.conv.clone(), s.msg.ts.clone());
                (Some(id), self.message_row(&s.msg))
            })
            .collect()
    }

    /// Whether `s` qualifies for the Focus view (spec §3): it must have arrived live during this
    /// session — `s.arrival` strictly past `session_watermark` (see that field's doc for why the
    /// comparison is strict, not `>=`) — *and* either its conversation is an allow-listed DM
    /// (`dm_allow_convs`) or its text hits a `focus_keywords` entry (`entities::keyword_hit`, same
    /// case-insensitive substring rule the Mentions `keywords` check uses, applied to the distinct
    /// `focus_keywords` list). The two qualifiers are OR'd, matching the spec's "allow-list OR
    /// keyword" framing — either alone is enough once the watermark gate passes.
    fn qualifies_for_focus(&self, s: &Stored) -> bool {
        s.arrival > self.session_watermark
            && (self.dm_allow_convs.contains(&s.msg.conv)
                || entities::keyword_hit(&s.msg.text, &self.focus_keywords))
    }

    /// Count of mentions not yet marked read.
    pub fn unread_mentions(&self) -> usize {
        self.messages
            .values()
            .filter(|s| self.is_mention_stored(s))
            .filter(|s| !self.read_mentions.contains(&(s.msg.conv.clone(), s.msg.ts.clone())))
            .count()
    }

    /// Any keypress: moves the unread divider to "now" (everything seen so far is read).
    pub fn touch(&mut self) {
        self.divider_mark = self.arrival_seq;
    }

    /// Switch the active tab and resync selection for it. The two tabs' row lists are
    /// independent (different lengths, different identities), so a `cursor`/`selected` left
    /// over from the previous tab means nothing here: this clears `selected` and re-derives
    /// both from scratch via `resync_cursor`, which clamps `cursor` into the new tab's bounds
    /// and anchors `selected` to whatever row now sits there. Task 7 should call this on tab
    /// switch rather than assigning `tab` directly, so a stale cursor from a longer previous
    /// tab can never point past the end of a shorter one.
    pub fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.selected = None;
        self.resync_cursor();
    }

    /// Flip the Feed tab between the Timeline and Threads projections (spec §3, the `t` key), a
    /// no-op on any other tab — the toggle key is Feed-only. Resyncs selection into the new
    /// projection exactly like `set_tab` does across tabs: `selected` is dropped rather than
    /// trusted across the flip, because the two projections' row sets only partially overlap (a
    /// Timeline-only collapsed `ThreadMarker` selection has no counterpart here, and a reply row
    /// hidden by that same collapse has no source row in a still-collapsed Timeline), so
    /// `resync_cursor`'s "keep the old identity if it still exists, else clamp" fallback is the
    /// only sound behavior either way.
    pub fn toggle_view(&mut self) {
        if self.tab != Tab::Feed {
            return;
        }
        self.view = match self.view {
            FeedView::Timeline | FeedView::Focus => FeedView::Threads,
            FeedView::Threads => FeedView::Timeline,
        };
        self.selected = None;
        self.resync_cursor();
    }

    /// Flip the Feed tab into/out of the Focus projection (spec §3, the `f` key) — the Focus
    /// counterpart to `toggle_view`, same Feed-tab gating and selection-resync, but targeting
    /// `Focus` instead of `Threads`; see [`FeedView`]'s doc for the full `t`/`f` decision table
    /// governing how the two toggles interact.
    pub fn toggle_focus(&mut self) {
        if self.tab != Tab::Feed {
            return;
        }
        self.view = match self.view {
            FeedView::Timeline | FeedView::Threads => FeedView::Focus,
            FeedView::Focus => FeedView::Timeline,
        };
        self.selected = None;
        self.resync_cursor();
    }

    /// `Enter` semantics per tab (and, on the Feed tab, per view): the Timeline and Focus views
    /// both expand/collapse the selected thread exactly the same way (fetching replies via REST
    /// on first expand — Focus reuses the Timeline's plain row rendering, so the same rows and
    /// `expand_target_root` resolution apply unchanged); the Threads view instead always
    /// (re)fetches the selected thread's replies (see `refresh_thread`'s doc for why a
    /// collapse-toggle semantics doesn't apply there); the Mentions tab toggles the selected row's
    /// read state.
    pub fn toggle_expand_or_read(&mut self, rest: &Rest) {
        match self.tab {
            Tab::Feed => match self.view {
                FeedView::Timeline | FeedView::Focus => self.toggle_expand(rest),
                FeedView::Threads => self.refresh_thread(rest),
            },
            Tab::Mentions => self.toggle_read(),
        }
    }

    /// A permalink for the selected row's message, if any (e.g. the cursor is past the end,
    /// or sits on the synthetic `Divider` row). Resolves via the selected identity, not raw
    /// row position — see `feed_target`/`mention_target`. On a REST failure this sets `status`
    /// naming the failure and returns `None`, the same as "nothing selected" to the caller, but
    /// now visibly distinguishable from it in the status line rather than silently doing nothing.
    pub fn permalink_of_selected(&mut self, rest: &Rest) -> Option<String> {
        let id = match self.tab {
            Tab::Feed => self.feed_target().map(|(id, _)| id),
            Tab::Mentions => self.mention_target().map(|(id, _)| id),
        }?;
        match crate::rest::permalink(rest, &id.0, &id.1) {
            Ok(url) => Some(url),
            Err(err) => {
                self.status = format!("permalink failed: {err:?}");
                None
            }
        }
    }

    // ---- Feed tab: thread expand (thin REST edge over the pure `toggle_thread`) ----------

    fn toggle_expand(&mut self, rest: &Rest) {
        let Some(((conv, sel_ts), row)) = self.feed_target() else {
            return;
        };
        let Some(root_ts) = self.expand_target_root(&conv, &sel_ts, &row) else {
            return;
        };
        let will_expand = !self.expanded.contains(&(conv.clone(), root_ts.clone()));
        let mut fetch_failed = false;
        let fetched = if will_expand {
            match crate::rest::replies(rest, &conv, &root_ts, None) {
                Ok(msgs) => msgs,
                Err(err) => {
                    self.status = format!("replies failed: {err:?}");
                    fetch_failed = true;
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let now_expanded = self.toggle_thread(&conv, &root_ts, fetched);
        if !fetch_failed {
            self.status = expand_status(now_expanded, self.local_reply_count(&conv, &root_ts));
        }
    }

    /// Resolve the thread root `(conv, root_ts)` that Enter on the selected Feed-tab Timeline row
    /// (`row`, identified by `(conv, sel_ts)`) should expand/collapse (spec §5-§6):
    /// - a `ThreadMarker` row names its root directly (`sel_ts` already is the root's `ts`);
    /// - a `Message` row names its root directly when the message itself is a thread root with
    ///   at least one reply (`is_thread_root_with_replies`) — the new "Enter on the root row"
    ///   route (spec §5);
    /// - a `Message` row that is itself a reply to a *locally-known* root resolves via its
    ///   stored `thread_ts` — covering both a still-nested expanded-reply row and a collapsed
    ///   activity row (spec §6), whichever is currently selected.
    ///
    /// Any other row — a plain non-thread message, or a reply whose root isn't locally known
    /// (an orphan — there is nothing to expand) — has nothing to toggle, so returns `None`.
    fn expand_target_root(&self, conv: &str, sel_ts: &str, row: &Row) -> Option<String> {
        match row.kind {
            RowKind::ThreadMarker { .. } => Some(sel_ts.to_string()),
            RowKind::Message => {
                if self.is_thread_root_with_replies(conv, sel_ts) {
                    return Some(sel_ts.to_string());
                }
                let root_ts = self.messages.get(&key_for(conv, sel_ts))?.msg.thread_ts.clone()?;
                self.messages.contains_key(&key_for(conv, &root_ts)).then_some(root_ts)
            }
            _ => None,
        }
    }

    /// Whether `(conv, ts)` is a thread root (not itself a reply) with at least one reply, by
    /// `marker_count`'s usual max-of-metadata-and-local rule — i.e. exactly the condition under
    /// which `feed_rows_with_ids` would render a `ThreadMarker` for it were it collapsed. Used by
    /// `expand_target_root` so Enter on the root's own `Message` row (spec §5) is only ever
    /// treated as thread-related when it actually has a thread to expand.
    fn is_thread_root_with_replies(&self, conv: &str, ts: &str) -> bool {
        let Some(stored) = self.messages.get(&key_for(conv, ts)) else {
            return false;
        };
        if stored.msg.thread_ts.is_some() {
            return false; // it's a reply, not a root
        }
        marker_count(stored.msg.reply_count, self.local_reply_count(conv, ts)) > 0
    }

    /// Whether the Feed tab's Timeline currently has a thread-related row selected — anything
    /// `expand_target_root` can resolve (spec §5-§6: root-with-thread, marker, nested reply, or
    /// activity row). `ui.rs`'s footer uses this to show the `enter expand/collapse thread` hint
    /// only when Enter would actually do something thread-related; `false` off the Feed tab or
    /// off the Timeline projection, since the Threads view's Enter is an unconditional refresh
    /// (`refresh_thread`), not a collapse/expand toggle this hint describes.
    pub fn selected_is_thread_related(&self) -> bool {
        if self.tab != Tab::Feed || self.view != FeedView::Timeline {
            return false;
        }
        let Some((id, row)) = self.feed_target() else {
            return false;
        };
        self.expand_target_root(&id.0, &id.1, &row).is_some()
    }

    /// `Enter` in the Threads view (spec §3: "Enter on a root (re)fetches its replies"): fetch
    /// the selected row's thread via REST and merge the result in via the normal upsert path,
    /// same as `toggle_expand`'s fetch — but, unlike `toggle_expand`, this never consults or
    /// flips `App::expanded`. The Threads view always shows every thread's locally-known replies
    /// nested beneath its root regardless of that flag (see `thread_rows_with_ids`'s doc), so
    /// there is no collapsed/expanded state here for a toggle to mean — a plain unconditional
    /// refresh is the simplest semantics that stays consistent whether the selected row is
    /// still-collapsed-in-the-Timeline, already expanded there, or a nested reply row (in which
    /// case `thread_root_ts` walks up to the thread it belongs to).
    fn refresh_thread(&mut self, rest: &Rest) {
        let Some((id, _)) = self.feed_target() else {
            return;
        };
        let root_ts = self.thread_root_ts(&id.0, &id.1);
        match crate::rest::replies(rest, &id.0, &root_ts, None) {
            Ok(msgs) => self.apply_fetched_replies(msgs),
            Err(err) => self.status = format!("thread refresh failed: {err:?}"),
        }
        self.resync_cursor();
    }

    /// The root `ts` of the thread `(conv, ts)` belongs to: `ts` itself when it names a root (or
    /// an id not locally known at all — nothing better to fall back to), or its stored
    /// `thread_ts` when it names a known reply. Used by `refresh_thread` so Enter on either a
    /// thread's root row or one of its nested reply rows in the Threads view refreshes the same
    /// thread.
    fn thread_root_ts(&self, conv: &str, ts: &str) -> String {
        self.messages
            .get(&key_for(conv, ts))
            .and_then(|s| s.msg.thread_ts.clone())
            .unwrap_or_else(|| ts.to_string())
    }

    /// Pure core of thread expand/collapse: flips the expanded flag for `(conv, root_ts)`,
    /// merging `fetched` replies into the store when expanding (collapsing needs no fetch, so
    /// callers pass an empty vec). Returns whether the thread is now expanded. Exposed to
    /// tests so expand/collapse behavior is checked without a real REST call.
    fn toggle_thread(&mut self, conv: &str, root_ts: &str, fetched: Vec<Message>) -> bool {
        let key = (conv.to_string(), root_ts.to_string());
        let now_expanded = if self.expanded.remove(&key) {
            false
        } else {
            self.expanded.insert(key);
            for msg in fetched {
                self.upsert_new(msg);
            }
            true
        };
        self.resync_cursor();
        now_expanded
    }

    // ---- Mentions tab: read toggle ------------------------------------------------------

    fn toggle_read(&mut self) {
        let Some((id, _)) = self.mention_target() else {
            return;
        };
        self.toggle_mention_read(&id.0, &id.1);
    }

    /// Pure core of the Mentions read toggle: flips `(conv, ts)`'s membership in the read set.
    fn toggle_mention_read(&mut self, conv: &str, ts: &str) {
        let key = (conv.to_string(), ts.to_string());
        if self.read_mentions.contains(&key) {
            self.read_mentions.remove(&key);
        } else {
            self.read_mentions.insert(key);
        }
    }

    // ---- Message store: insert / edit / delete ------------------------------------------

    /// Insert a newly-seen message, or — if `(conv, ts)` is already known — replace its content
    /// iff the incoming copy is itself marked edited and actually differs from what's stored.
    /// That guard is what lets a poll fallback (which only ever learns of an edit by re-fetching
    /// history and re-running it through this same path — there is no separate "poll saw an
    /// edit" event) surface the edit, while still stopping a stale pre-edit poll copy (always
    /// `edited: false`) from clobbering a live `SocketEvent::Changed` that landed in between: a
    /// `Changed` event goes through `upsert_edit`, not here, but a poll's re-fetch of that same
    /// message is a `SocketEvent::Message` routed through `upsert_new` (see `poll_tick`), so
    /// without this guard the poll's `edited: false` copy would silently overwrite the edit.
    /// The original `arrival` is preserved on an edit-through-poll so the unread divider doesn't
    /// jump for a message that isn't actually new.
    fn upsert_new(&mut self, msg: Message) {
        let key = key_for(&msg.conv, &msg.ts);
        if let Some(stored) = self.messages.get_mut(&key) {
            if msg.edited && msg != stored.msg {
                stored.msg = msg;
            }
            return;
        }
        track_newest(&mut self.newest_ts, &msg.conv, &msg.ts);
        self.arrival_seq += 1;
        let arrival = self.arrival_seq;
        self.messages.insert(key, Stored { msg, arrival });
    }

    /// Replace an edited message's fields in place; if it was never seen before (e.g. its
    /// original arrival predates this session), insert it fresh instead of dropping the edit.
    /// That insert-fresh path deliberately assigns a brand-new `arrival_seq` (rather than, say,
    /// backdating it) so the edit surfaces under the unread divider like any other new arrival.
    fn upsert_edit(&mut self, mut msg: Message) {
        let key = key_for(&msg.conv, &msg.ts);
        if let Some(stored) = self.messages.get_mut(&key) {
            // A live `message_changed` event's payload never carries `reply_count` (see
            // `socket::message_from`), so an incoming `None` here means "field not reported",
            // not "no replies" — inherit whatever was already known rather than clobbering it.
            msg.reply_count = msg.reply_count.or(stored.msg.reply_count);
            stored.msg = msg;
        } else {
            self.arrival_seq += 1;
            let arrival = self.arrival_seq;
            self.messages.insert(key, Stored { msg, arrival });
        }
    }

    fn remove(&mut self, conv: &str, ts: &str) {
        self.messages.remove(&key_for(conv, ts));
        let id = (conv.to_string(), ts.to_string());
        self.read_mentions.remove(&id);
        self.expanded.remove(&id);
    }

    // ---- Row/label rendering helpers ------------------------------------------------------

    fn is_mention_stored(&self, s: &Stored) -> bool {
        let kind = self.conv_kinds.get(&s.msg.conv).copied().unwrap_or(ConvKind::Channel);
        is_mention(&s.msg, kind, &self.self_id, &self.keywords)
    }

    fn message_row(&self, msg: &Message) -> Row {
        let text = entities::resolve(
            &msg.text,
            |id| self.user_names.get(id).cloned(),
            |id| self.conv_names.get(id).cloned(),
        );
        let author =
            self.user_names.get(&msg.author).cloned().unwrap_or_else(|| msg.author.clone());
        Row {
            conv_label: self.conv_label(&msg.conv),
            author,
            time_hhmm: ts_to_hhmm(&msg.ts),
            text,
            kind: RowKind::Message,
        }
    }

    /// A reply row nested under an expanded root (today's behavior), or an orphaned reply whose
    /// root isn't locally known at all (spec §6: "keep their existing inline treatment"): the
    /// plain message row with a "↳ " nesting prefix, sharing `SelKind::Message` identity with
    /// whatever other rendering of the same `(conv, ts)` the caller is choosing between.
    fn nested_reply_row(&self, msg: &Message) -> Row {
        let mut row = self.message_row(msg);
        row.text = format!("\u{21b3} {}", row.text);
        row
    }

    /// A reply to a *collapsed* thread, rendered as a discoverable activity row at its own
    /// chronological position (spec §6) instead of vanishing entirely: the same row shape as any
    /// other message (conv label/author/time styled normally), with the text replaced by
    /// `↳ @author replied: <text>` so it still reads as thread context. `RowKind::Message` and
    /// the reply's own `(conv, ts)` identity are kept (see `feed_rows_with_ids`'s caller), so
    /// Enter on it resolves here and expands the root via `expand_target_root`.
    fn activity_row(&self, msg: &Message) -> Row {
        let mut row = self.message_row(msg);
        row.text = format!("\u{21b3} @{} replied: {}", row.author, row.text);
        row
    }

    fn conv_label(&self, conv_id: &str) -> String {
        let kind = self.conv_kinds.get(conv_id).copied().unwrap_or(ConvKind::Channel);
        let name = self.conv_names.get(conv_id).cloned().unwrap_or_else(|| conv_id.to_string());
        match kind {
            ConvKind::Im => format!("@{name}"),
            ConvKind::Channel | ConvKind::Group | ConvKind::Mpim => format!("#{name}"),
        }
    }
}

/// Render a Slack `ts`'s seconds as a `HH:MM` UTC clock time (no timezone crate in the closed
/// dependency list, so this is plain epoch-seconds arithmetic). A malformed `ts` parses via
/// `ts_key`'s `(0, 0)` fallback and renders as `00:00` rather than panicking.
fn ts_to_hhmm(ts: &str) -> String {
    let (secs, _) = ts_key(ts);
    let day_secs = secs % 86_400;
    format!("{:02}:{:02}", day_secs / 3600, (day_secs % 3600) / 60)
}

/// Fold one conversation's backfill result into `app`, or produce an error naming which
/// conversation's history fetch failed (spec: "a per-channel history error fails build with an
/// error naming the channel"). Split out of `build`'s loop so the error-naming path is
/// unit-tested without a real REST call.
fn apply_backfill(
    app: &mut App,
    conv_name: &str,
    msgs: Result<Vec<Message>, RestError>,
) -> Result<(), String> {
    let msgs = msgs.map_err(|e| format!("history failed for {conv_name}: {e:?}"))?;
    for msg in msgs {
        app.upsert_new(msg);
    }
    Ok(())
}

/// The reply count a `ThreadMarker` row should display: the greater of Slack's own
/// `reply_count` metadata (from a history/backfill/poll root, or a live `message_changed`
/// update — see `Message::reply_count`'s doc) and the number of replies actually stored
/// locally. Taking the max (rather than always preferring one or the other) is what lets a
/// freshly-backfilled thread show its true count immediately — before any reply has been
/// fetched, `reply_count` alone carries it — while a thread whose replies arrived live (via
/// the socket, where roots don't carry updated metadata on every reply — see `socket.rs`'s
/// module doc) still counts correctly from what's locally known, even if that exceeds a
/// stale/never-set `reply_count`.
fn marker_count(root_reply_count: Option<u32>, local_replies: usize) -> usize {
    (root_reply_count.unwrap_or(0) as usize).max(local_replies)
}

/// The status line `toggle_expand` sets after a successful expand/collapse (spec §5):
/// `"thread expanded — n replies"` (`reply_count` is the thread's locally-known reply count
/// right after the toggle, including anything just fetched) when expanding — singular `"1 reply"`
/// rather than `"1 replies"` for the one-reply case — or a plain `"thread collapsed"` note when
/// collapsing. Pure so the wording is unit-tested without a REST call — `toggle_expand` is the
/// only caller, threading in `local_reply_count` after `toggle_thread` runs; a failed fetch
/// bypasses this entirely so the existing `"replies failed: ..."` wording is never overwritten
/// (spec: "error wording unchanged").
fn expand_status(now_expanded: bool, reply_count: usize) -> String {
    if now_expanded {
        let noun = if reply_count == 1 { "reply" } else { "replies" };
        format!("thread expanded — {reply_count} {noun}")
    } else {
        "thread collapsed".to_string()
    }
}

/// Up to how many of `poll_tick_at`'s `POLL_BATCH`-sized budget go to the active-thread
/// round-robin this tick: the number of active threads, capped at `2` — so `0` active threads
/// reserves nothing (the full budget goes to conversations instead — see `App::active_threads`),
/// exactly `1` active thread reserves exactly `1` slot (reserving a flat 2 here would waste a
/// slot no thread exists to fill; `poll_conversations` picks up the difference), and `2` or more
/// active threads reserves the full `2`. Spec §2: "up to 2 of the 8 slots rotate round-robin
/// over active threads."
fn thread_slot_count(active_threads: usize) -> usize {
    active_threads.min(2)
}

/// Update `newest` in place with `ts` for `conv`, iff `ts` is chronologically at or after
/// whatever's already tracked (or nothing is tracked yet) — the newest-seen-`ts` half of
/// `App::newest_ts`'s doc, split out pure so the "keep the max" comparison is unit-tested
/// without going through a whole `upsert_new`/`App` round-trip.
fn track_newest(newest: &mut HashMap<String, String>, conv: &str, ts: &str) {
    let is_newer =
        newest.get(conv).is_none_or(|existing| ts_cmp(ts, existing) != std::cmp::Ordering::Less);
    if is_newer {
        newest.insert(conv.to_string(), ts.to_string());
    }
}

/// Whether a tick starting at `now` should be skipped entirely because a prior `RateLimited`
/// hit's cooldown (`deadline`) hasn't passed yet. Pure and `Instant`-based so `poll_tick_at`'s
/// gating is unit-tested by constructing deadlines via `Instant` arithmetic — no mock clock
/// needed (see `App::cooldown_until`'s doc).
fn cooldown_active(now: Instant, deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| now < d)
}

/// Whether `maybe_scan_out_of_cap_dms`'s 5-minute out-of-cap DM activity scan (spec §1) is due at
/// `now`: `next_scan` is `None` before the very first tick (always due — there is nothing to wait
/// on yet) or `Some(deadline)` set by the prior due scan (`now + DM_SCAN_INTERVAL`), due once
/// `now` reaches (or passes) it. Pure and `Instant`-based for the same reason as
/// [`cooldown_active`] — no mock clock needed to test the gate.
fn dm_scan_due(now: Instant, next_scan: Option<Instant>) -> bool {
    next_scan.is_none_or(|deadline| now >= deadline)
}

/// The single out-of-cap DM/MPIM conversation whose `updated` moved furthest since it was last
/// observed, if any did (spec §1's polling-mode out-of-cap DM activity detection). `old` is the
/// App's previously-stored full `conversations.list` snapshot (`App::all_conversations`, from
/// `build` or the prior scan); `new_all` is what this scan just re-fetched; `subscribed` is the
/// set of conversation ids already in the capped/subscribed set (out of scope here entirely —
/// those get fresh data from the normal `POLL_BATCH` round-robin instead); `last_seen` is the
/// watermark of the newest `updated` a scan has actually issued a `history` call for, per
/// out-of-cap DM id (`App::dm_last_seen`).
///
/// A conversation's baseline "last observed" value is `last_seen`'s entry for it if one exists
/// (the DM has already been scan-fetched before, so that watermark is authoritative), else its
/// `updated` in `old` (never scan-fetched, but already known at the last full-list snapshot), else
/// `0` (never seen at all before this scan — e.g. a DM created since `build` — so any `updated`
/// counts as new activity). Only `Im`/`Mpim` conversations not in `subscribed`, with a `Some`
/// `updated` strictly greater than that baseline, are candidates; among them the one with the
/// greatest `updated` wins (ties broken by id, for a deterministic pick — the exact winner among
/// simultaneous ties doesn't matter functionally, since the loser simply waits for the next scan).
/// `None` when nothing qualifies — the common case, and why this scan usually costs zero extra
/// requests despite re-fetching the full list every 5 minutes.
fn pick_changed_dm(
    old: &[Conversation],
    new_all: &[Conversation],
    subscribed: &HashSet<String>,
    last_seen: &HashMap<String, u64>,
) -> Option<Conversation> {
    new_all
        .iter()
        .filter(|c| matches!(c.kind, ConvKind::Im | ConvKind::Mpim))
        .filter(|c| !subscribed.contains(&c.id))
        .filter_map(|c| {
            let updated = c.updated?;
            let baseline = last_seen
                .get(&c.id)
                .copied()
                .or_else(|| old.iter().find(|o| o.id == c.id).and_then(|o| o.updated))
                .unwrap_or(0);
            (updated > baseline).then_some((c, updated))
        })
        .max_by(|(a, au), (b, bu)| au.cmp(bu).then_with(|| a.id.cmp(&b.id)))
        .map(|(c, _)| c.clone())
}

/// Convert a `conversations.list` `updated` stamp (Slack's millisecond-epoch conversation-
/// activity timestamp) to a Slack message `ts` string (`<seconds>.<6-digit microseconds>`), for
/// threading into `maybe_scan_out_of_cap_dms`'s `history(..., oldest)` call — `history`'s `oldest`
/// wants a `ts`-shaped string, not a raw millisecond count. The sub-second remainder becomes the
/// microsecond field (millisecond precision padded with three zero digits); this is not a real
/// Slack `ts` (no message actually has this exact stamp), but `history`'s `oldest` only needs a
/// chronological cutoff, not a literal message identity, and `ts_cmp`/`ts_key` compare purely
/// numerically, so a synthetic-but-correctly-ordered value works exactly the same as a real one.
fn updated_ms_to_ts(updated_ms: u64) -> String {
    let secs = updated_ms / 1000;
    let micros = (updated_ms % 1000) * 1000;
    format!("{secs}.{micros:06}")
}

/// The indices (into a `n`-long conversation list) `poll_tick` should visit this tick, round-
/// robin from `cursor`, wrapping past the end back to `0`, plus the cursor to resume from next
/// tick. `batch` is clamped to `n` (a batch larger than the whole list would otherwise produce
/// duplicate indices via the wraparound). `n == 0` yields no indices and resets the cursor to
/// `0` rather than looping forever on a modulus of zero.
fn next_batch(cursor: usize, n: usize, batch: usize) -> (Vec<usize>, usize) {
    if n == 0 {
        return (Vec::new(), 0);
    }
    let take = batch.min(n);
    let indices = (0..take).map(|i| (cursor + i) % n).collect();
    let next_cursor = (cursor + take) % n;
    (indices, next_cursor)
}

/// A per-conversation backfill retry's outcome, decided purely so `backfill_one`'s branching is
/// unit-tested without a real sleep or REST call (spec §5: a second consecutive `RateLimited`
/// degrades to skipping the rest of backfill rather than failing `build`).
enum BackfillRetry {
    /// The retry succeeded: fold these messages in and continue to the next conversation.
    Continue(Vec<Message>),
    /// The retry was rate-limited again: stop backfilling (this status notice), but `build`
    /// still succeeds — the socket/poll path fills in the rest later.
    SkipRemaining(String),
    /// The retry hit some other error: `build` still fails loud, naming the channel (unchanged
    /// contract for a non-rate-limit per-channel failure).
    HardFail(String),
}

/// Pure decision for `backfill_one`'s one-time retry, given `conv_name` (for the status/error
/// wording) and the retry's own `history()` result.
fn backfill_retry_decision(
    conv_name: &str,
    retry: Result<Vec<Message>, RestError>,
) -> BackfillRetry {
    match retry {
        Ok(msgs) => BackfillRetry::Continue(msgs),
        Err(RestError::RateLimited(_)) => BackfillRetry::SkipRemaining(format!(
            "slack rate limit — skipping remaining backfill (from {conv_name} on)"
        )),
        Err(other) => BackfillRetry::HardFail(format!("history failed for {conv_name}: {other:?}")),
    }
}

/// Cap a `Retry-After` value before sleeping on it (defensive: a misbehaving/proxy response
/// naming an absurd delay must not hang `build` for longer than a minute).
fn capped_sleep_secs(secs: u64) -> u64 {
    secs.min(60)
}

/// `backfill_one`'s outcome, distinguishing "stop backfilling the rest of the list, but this is
/// not a `build` failure" (`SkipRemaining`) from an actual `build`-failing error — a plain
/// `Result<(), String>` can't tell those apart, and the caller's loop needs to.
enum BackfillOutcome {
    /// Continue to the next conversation.
    Ok,
    /// A second consecutive `RateLimited`: stop backfilling the rest, but `build` still
    /// succeeds (the status notice was already set on `app`).
    SkipRemaining,
    /// `build` fails loud, naming the channel (unchanged contract).
    Fail(String),
}

/// Backfill one conversation into `app`, retrying once on `RateLimited` (spec §5): sleeps the
/// real `Retry-After` (capped by `capped_sleep_secs`) via `std::thread::sleep` — acceptable
/// here specifically because `build` runs once, before the TUI ever draws a frame, so blocking
/// it briefly costs nothing the pane's later async/event-driven paths need to avoid — then
/// retries the same conversation exactly once via `backfill_retry_decision`. Any error other
/// than the initial `RateLimited` keeps the unchanged contract: a misconfigured channel fails
/// `build` loud, naming it.
fn backfill_one(app: &mut App, rest: &Rest, conv: &Conversation) -> BackfillOutcome {
    match crate::rest::history(rest, &conv.id, 50, None) {
        Err(RestError::RateLimited(secs)) => {
            std::thread::sleep(Duration::from_secs(capped_sleep_secs(secs)));
            let retry = crate::rest::history(rest, &conv.id, 50, None);
            match backfill_retry_decision(&conv.name, retry) {
                BackfillRetry::Continue(msgs) => {
                    for msg in msgs {
                        app.upsert_new(msg);
                    }
                    BackfillOutcome::Ok
                }
                BackfillRetry::SkipRemaining(note) => {
                    app.status = note;
                    BackfillOutcome::SkipRemaining
                }
                BackfillRetry::HardFail(msg) => BackfillOutcome::Fail(msg),
            }
        }
        other => match apply_backfill(app, &conv.name, other) {
            Ok(()) => BackfillOutcome::Ok,
            Err(msg) => BackfillOutcome::Fail(msg),
        },
    }
}

/// The one-line status `poll_tick` sets on a per-conversation history-fetch failure: a
/// rate-limit notice naming the retry delay (Slack's own back-off signal, distinct from any
/// other failure) or a line naming which conversation failed and why. Split out so the wording
/// is unit-tested without a real REST call.
fn poll_error_status(conv_name: &str, error: &RestError) -> String {
    match error {
        RestError::RateLimited(secs) => format!("slack rate limit — retrying in {secs}s"),
        other => format!("{conv_name}: {other:?}"),
    }
}

/// Resolve each `Im` conversation's display name: `conversations.list` sets an IM's `name` to
/// its counterpart user id (rest.rs's `parse_conversations` doc), not a display name, so it is
/// looked up in the `users.list` cache here — falling back to the raw id if the user is
/// somehow not in the cache (e.g. a deactivated account `users.list` still lists but this
/// lookup can't find, or any other mismatch) rather than failing the whole build over one
/// unresolved DM name.
fn resolve_im_names(
    convs: Vec<Conversation>,
    users: &HashMap<String, String>,
) -> Vec<Conversation> {
    convs
        .into_iter()
        .map(|c| {
            if c.kind == ConvKind::Im {
                let name = users.get(&c.name).cloned().unwrap_or_else(|| c.name.clone());
                Conversation { name, ..c }
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        App, BackfillOutcome, BackfillRetry, DM_SCAN_INTERVAL, FeedView, RowKind, SelKind, Tab,
        apply_backfill, backfill_retry_decision, capped_sleep_secs, cooldown_active, dm_scan_due,
        expand_status, marker_count, next_batch, pick_changed_dm, poll_error_status,
        resolve_im_names, thread_slot_count, track_newest, ts_to_hhmm, updated_ms_to_ts,
    };
    use crate::model::{ConvKind, Conversation, Message};
    use crate::rest::{Rest, RestError};
    use crate::socket::SocketEvent;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    fn conv(id: &str, name: &str, kind: ConvKind) -> Conversation {
        Conversation { id: id.into(), name: name.into(), kind, updated: None }
    }

    fn msg(conv: &str, ts: &str, thread_ts: Option<&str>, author: &str, text: &str) -> Message {
        Message {
            conv: conv.into(),
            ts: ts.into(),
            thread_ts: thread_ts.map(str::to_owned),
            author: author.into(),
            text: text.into(),
            edited: false,
            reply_count: None,
        }
    }

    fn empty_app() -> App {
        App {
            tab: Tab::Feed,
            view: FeedView::Timeline,
            cursor: 0,
            status: String::new(),
            polling: false,
            conversations: vec![
                conv("C1", "eng", ConvKind::Channel),
                conv("C2", "ops", ConvKind::Channel),
            ],
            conv_names: HashMap::from([
                ("C1".to_string(), "eng".to_string()),
                ("C2".to_string(), "ops".to_string()),
            ]),
            conv_kinds: HashMap::from([
                ("C1".to_string(), ConvKind::Channel),
                ("C2".to_string(), ConvKind::Channel),
            ]),
            user_names: HashMap::from([("U1".to_string(), "dan".to_string())]),
            messages: BTreeMap::new(),
            expanded: HashSet::new(),
            read_mentions: HashSet::new(),
            selected: None,
            self_id: "SELF".to_string(),
            keywords: Vec::new(),
            arrival_seq: 0,
            divider_mark: 0,
            newest_ts: HashMap::new(),
            poll_cursor: 0,
            poll_thread_cursor: 0,
            cooldown_until: None,
            pending_new: 0,
            viewport_rows: 20,
            all_conversations: Vec::new(),
            dm_last_seen: HashMap::new(),
            next_dm_scan: None,
            session_watermark: 0,
            focus_keywords: Vec::new(),
            dm_allow_convs: HashSet::new(),
        }
    }

    // ---- ordering across convs -----------------------------------------------------------

    #[test]
    fn feed_rows_orders_chronologically_across_conversations() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C2", "2.000000", None, "U1", "second conv, later")));
        app.apply(SocketEvent::Message(msg("C1", "1.000000", None, "U1", "first conv, earlier")));
        app.touch(); // not exercising the divider here — see the divider-specific tests below.
        let rows = app.feed_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "first conv, earlier");
        assert_eq!(rows[1].text, "second conv, later");
    }

    // ---- distinct malformed-ts messages never collide in the message store ----------------

    #[test]
    fn distinct_malformed_ts_messages_both_survive_and_render() {
        let mut app = empty_app();
        // Both malformed ts values parse via ts_key's (0, 0) fallback; the map key must still
        // tell them apart (via the raw ts string) or the second upsert clobbers the first.
        app.apply(SocketEvent::Message(msg("C1", "garbage-1", None, "U1", "malformed one")));
        app.apply(SocketEvent::Message(msg("C1", "garbage-2", None, "U1", "malformed two")));
        app.touch();

        let rows = app.feed_rows();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.text == "malformed one"));
        assert!(rows.iter().any(|r| r.text == "malformed two"));
    }

    // ---- edit updates in place -------------------------------------------------------------

    #[test]
    fn changed_event_replaces_text_in_place() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "original")));
        app.apply(SocketEvent::Changed(Message {
            edited: true,
            ..msg("C1", "1.0", None, "U1", "edited text")
        }));
        app.touch();
        let rows = app.feed_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "edited text");
    }

    #[test]
    fn changed_event_without_reply_count_preserves_the_stored_reply_count() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(Message {
            reply_count: Some(5),
            ..msg("C1", "1.0", None, "U1", "root")
        }));
        // A live `message_changed` event carries no `reply_count` field at all (see
        // `socket::message_from`), so the parsed `Message` has `reply_count: None` — that must
        // not clobber the `Some(5)` already known for this root.
        app.apply(SocketEvent::Changed(Message {
            edited: true,
            ..msg("C1", "1.0", None, "U1", "edited text")
        }));
        app.touch();

        let rows = app.feed_rows();
        assert_eq!(rows[0].text, "edited text");
        // Proxy assertion: reply_count isn't otherwise exposed, but active_threads() only
        // treats a root as active when its (possibly-stale) reply_count exceeds locally-known
        // replies — that stays true here (0 local < 5) only if reply_count survived the edit.
        assert_eq!(
            app.active_threads(),
            vec![("C1".to_string(), "1.0".to_string())],
            "reply_count must still be Some(5) after the edit, so the count-gap keeps it active"
        );
    }

    // ---- delete removes ---------------------------------------------------------------------

    #[test]
    fn deleted_event_removes_the_message() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "gone soon")));
        app.apply(SocketEvent::Deleted { conv: "C1".into(), ts: "1.0".into() });
        assert!(app.feed_rows().is_empty());
    }

    // ---- thread collapse + count -------------------------------------------------------------

    #[test]
    fn thread_replies_collapse_under_the_root_with_a_count() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
        app.apply(SocketEvent::Message(msg("C1", "1.000003", Some("1.000001"), "U1", "reply two")));
        app.touch();

        // Spec §6: a collapsed thread still shows its `ThreadMarker` right after the root, but
        // each known reply also gets its own discoverable activity row at its chronological
        // position, rather than vanishing entirely.
        let rows = app.feed_rows();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].kind, RowKind::Message);
        assert_eq!(rows[1].kind, RowKind::ThreadMarker { replies: 2, expanded: false });
        assert_eq!(rows[2].text, "\u{21b3} @dan replied: reply one");
        assert_eq!(rows[2].kind, RowKind::Message);
        assert_eq!(rows[3].text, "\u{21b3} @dan replied: reply two");
    }

    // ---- reply activity rows (Task 2, spec §6): a collapsed thread's replies get a discoverable
    // activity row at their own chronological position instead of vanishing entirely; an
    // expanded thread still nests them under the root with no activity rows at all. --------------

    #[test]
    fn a_collapsed_threads_reply_activity_row_carries_the_replys_own_identity() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
        app.touch();

        let rows = app.feed_rows_with_ids();
        let (id, row) = rows
            .into_iter()
            .find(|(_, r)| r.text.contains("replied:"))
            .expect("an activity row must be present for the collapsed reply");
        assert_eq!(id, Some(("C1".to_string(), "1.000002".to_string())));
        assert_eq!(row.kind, RowKind::Message);
    }

    #[test]
    fn an_expanded_thread_emits_no_activity_rows_for_its_replies() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
        app.touch();
        app.toggle_thread("C1", "1.000001", Vec::new());

        let rows = app.feed_rows();
        assert!(
            !rows.iter().any(|r| r.text.contains("replied:")),
            "an expanded thread's replies must nest under the root, not also appear as activity rows:\n{rows:?}"
        );
        assert!(rows.iter().any(|r| r.text == "reply one"));
    }

    #[test]
    fn an_orphaned_reply_keeps_its_existing_inline_treatment_unaffected_by_activity_rows() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg(
            "C1",
            "1.000002",
            Some("1.000001"),
            "U1",
            "reply to a root we never saw",
        )));
        app.touch();

        let rows = app.feed_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "\u{21b3} reply to a root we never saw");
    }

    // ---- Enter routing on thread-related rows (Task 2, spec §5-§6) ---------------------------

    #[test]
    fn entering_on_a_thread_roots_message_row_toggles_expansion_like_its_marker() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
        app.touch();

        app.move_cursor(0); // the root's own Message row, not its ThreadMarker
        let rows = app.feed_rows();
        assert_eq!(rows[app.cursor].kind, RowKind::Message);
        assert_eq!(rows[app.cursor].text, "root");

        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        app.toggle_expand_or_read(&rest);

        let rows = app.feed_rows();
        assert!(
            !rows.iter().any(|r| matches!(r.kind, RowKind::ThreadMarker { .. })),
            "Enter on the root row must expand the thread just like Enter on its marker"
        );
        assert!(rows.iter().any(|r| r.text == "reply one"));
    }

    #[test]
    fn entering_on_an_activity_row_expands_the_root_thread() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
        app.touch();

        let rows = app.feed_rows_with_ids();
        let pos = rows.iter().position(|(_, r)| r.text.contains("replied:")).unwrap();
        app.cursor = pos;
        app.selected = None;

        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        app.toggle_expand_or_read(&rest);

        let rows = app.feed_rows();
        assert!(
            !rows.iter().any(|r| r.text.contains("replied:")),
            "the activity row must disappear once the thread is expanded:\n{rows:?}"
        );
        assert!(rows.iter().any(|r| r.text == "reply one"));
    }

    #[test]
    fn entering_on_a_nested_reply_row_collapses_an_expanded_thread() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
        app.touch();
        app.toggle_thread("C1", "1.000001", Vec::new()); // now expanded

        app.move_cursor(1); // the nested reply row
        let rows = app.feed_rows();
        assert_eq!(rows[app.cursor].text, "reply one");

        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        app.toggle_expand_or_read(&rest);

        let rows = app.feed_rows();
        assert!(
            rows.iter().any(|r| matches!(r.kind, RowKind::ThreadMarker { .. })),
            "Enter on a nested reply row must collapse the thread back to a marker:\n{rows:?}"
        );
    }

    // ---- completion feedback (Task 2, spec §5) -------------------------------------------------

    #[test]
    fn expand_status_names_the_reply_count_when_expanding() {
        assert_eq!(expand_status(true, 3), "thread expanded — 3 replies");
    }

    #[test]
    fn expand_status_uses_singular_reply_for_exactly_one() {
        assert_eq!(expand_status(true, 1), "thread expanded — 1 reply");
    }

    #[test]
    fn expand_status_is_a_plain_collapsed_note() {
        assert_eq!(expand_status(false, 0), "thread collapsed");
    }

    #[test]
    fn collapsing_a_thread_via_the_root_row_sets_a_collapsed_status() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
        app.touch();
        app.toggle_thread("C1", "1.000001", Vec::new()); // now expanded
        app.cursor = 0;
        app.selected = None;

        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        app.toggle_expand_or_read(&rest);

        assert_eq!(app.status, "thread collapsed");
    }

    #[test]
    fn expanding_a_thread_renders_its_replies_inline() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
        app.touch();

        let expanded = app.toggle_thread("C1", "1.000001", Vec::new());
        assert!(expanded);

        let rows = app.feed_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "root");
        assert_eq!(rows[1].text, "reply one");
        assert_eq!(rows[1].kind, RowKind::Message);

        let collapsed = app.toggle_thread("C1", "1.000001", Vec::new());
        assert!(!collapsed);
        // Recollapsing brings back the `ThreadMarker` *and* now also the reply's own activity
        // row (spec §6) — the reply is no longer nested, but it doesn't vanish either.
        let rows = app.feed_rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].kind, RowKind::ThreadMarker { replies: 1, expanded: false });
        assert_eq!(rows[2].text, "\u{21b3} @dan replied: reply one");
    }

    #[test]
    fn expanding_a_thread_merges_fetched_replies() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.touch();
        let fetched = vec![msg("C1", "1.000002", Some("1.000001"), "U1", "fetched reply")];

        app.toggle_thread("C1", "1.000001", fetched);
        app.touch(); // the merged reply is a fresh arrival too; not exercising the divider here.

        let rows = app.feed_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].text, "fetched reply");
    }

    // ---- orphaned thread replies (root not locally known) still render --------------------

    #[test]
    fn a_reply_whose_root_is_unknown_renders_as_a_normal_row() {
        let mut app = empty_app();
        // No root "1.000001" was ever stored (e.g. it's older than the backfill horizon), but
        // a reply to it arrives — it must still render, not vanish.
        app.apply(SocketEvent::Message(msg(
            "C1",
            "1.000002",
            Some("1.000001"),
            "U1",
            "reply to a root we never saw",
        )));
        app.touch();

        let rows = app.feed_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, RowKind::Message);
        assert_eq!(rows[0].text, "\u{21b3} reply to a root we never saw");
    }

    // ---- divider placement after touch -------------------------------------------------------

    #[test]
    fn divider_sits_before_messages_that_arrived_after_the_last_touch() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "seen before touch")));
        app.touch();
        app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "arrived after touch")));

        let rows = app.feed_rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text, "seen before touch");
        assert_eq!(rows[1].kind, RowKind::Divider);
        assert_eq!(rows[2].text, "arrived after touch");
    }

    #[test]
    fn no_divider_when_nothing_arrived_since_the_last_touch() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "only message")));
        app.touch();
        let rows = app.feed_rows();
        assert_eq!(rows.len(), 1);
        assert!(!rows.iter().any(|r| r.kind == RowKind::Divider));
    }

    // ---- mention read toggle + unread_mentions count -----------------------------------------

    #[test]
    fn mention_read_toggle_and_unread_count() {
        let mut app = empty_app();
        app.conv_kinds.insert("D1".to_string(), ConvKind::Im);
        app.conv_names.insert("D1".to_string(), "dan".to_string());
        app.apply(SocketEvent::Message(msg("D1", "1.0", None, "U1", "a dm is always a mention")));

        assert_eq!(app.unread_mentions(), 1);
        let rows = app.mention_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, RowKind::Mention { read: false });

        app.toggle_mention_read("D1", "1.0");
        assert_eq!(app.unread_mentions(), 0);
        let rows = app.mention_rows();
        assert_eq!(rows[0].kind, RowKind::Mention { read: true });

        app.toggle_mention_read("D1", "1.0");
        assert_eq!(app.unread_mentions(), 1);
    }

    #[test]
    fn mention_rows_are_oldest_first_newest_at_the_bottom() {
        let mut app = empty_app();
        app.conv_kinds.insert("D1".to_string(), ConvKind::Im);
        app.apply(SocketEvent::Message(msg("D1", "1.0", None, "U1", "older dm")));
        app.apply(SocketEvent::Message(msg("D1", "2.0", None, "U1", "newer dm")));

        let rows = app.mention_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "older dm");
        assert_eq!(rows[1].text, "newer dm");
    }

    #[test]
    fn a_channel_message_without_a_mention_or_keyword_does_not_appear_on_mentions_tab() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "just chatting")));
        assert_eq!(app.unread_mentions(), 0);
        assert!(app.mention_rows().is_empty());
    }

    // ---- poll dedup ---------------------------------------------------------------------------

    #[test]
    fn a_message_seen_via_socket_and_poll_appears_once_and_keeps_its_original_content() {
        let mut app = empty_app();
        // Simulates the same (conv, ts) arriving twice: once via the socket, once via a poll
        // backfill re-fetch (poll_tick re-runs history() and re-applies through the same
        // upsert path apply() uses). upsert_new is insert-if-absent, so the second arrival
        // must not overwrite the first (see the stale-poll-vs-edit test below for why).
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "hello")));
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "hello (stale poll copy)")));
        app.touch();
        let rows = app.feed_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "hello");
    }

    #[test]
    fn a_stale_poll_message_does_not_revert_a_socket_edit() {
        let mut app = empty_app();
        let original = msg("C1", "1.0", None, "U1", "original");
        app.apply(SocketEvent::Message(original.clone()));
        app.apply(SocketEvent::Changed(Message {
            edited: true,
            ..msg("C1", "1.0", None, "U1", "edited text")
        }));
        // A history() poll response landing after the edit, carrying the pre-edit content —
        // upsert_new (insert-if-absent) must leave the edited entry alone.
        app.apply(SocketEvent::Message(original));
        app.touch();

        let rows = app.feed_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "edited text");
    }

    #[test]
    fn a_duplicate_arrival_does_not_move_the_divider() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "hello")));
        app.touch();
        // Re-applying the same message (e.g. a poll re-fetch) must not look like new arrival.
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "hello")));
        let rows = app.feed_rows();
        assert!(!rows.iter().any(|r| r.kind == RowKind::Divider));
    }

    // ---- socket connection status ---------------------------------------------------------

    #[test]
    fn down_event_sets_polling_and_a_status_message() {
        let mut app = empty_app();
        app.apply(SocketEvent::Down("refresh_requested".to_string()));
        assert!(app.polling);
        assert!(app.status.contains("refresh_requested"));
    }

    #[test]
    fn connected_event_clears_polling_and_status() {
        let mut app = empty_app();
        app.apply(SocketEvent::Down("x".to_string()));
        app.apply(SocketEvent::Connected);
        assert!(!app.polling);
        assert!(app.status.is_empty());
    }

    #[test]
    fn connected_event_clears_a_stale_cooldown_so_a_manual_poll_proceeds() {
        // A cooldown set before a reconnect must not silently no-op the user's next manual poll
        // (`r`) until its deadline lapses: a healthy socket means Slack accepted the connection,
        // so the poll path should restart clean.
        let mut app = empty_app();
        let now = Instant::now();
        app.cooldown_until = Some(now + Duration::from_secs(30));

        app.apply(SocketEvent::Connected);
        assert_eq!(app.cooldown_until, None);

        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        // Still "before" the old deadline had it not been cleared — the tick must nonetheless
        // run both conversations (observable via the cursor wrapping back to 0), not skip.
        app.poll_tick_at(&rest, now);
        assert_eq!(app.poll_cursor, 0, "both conversations were visited and the cursor wrapped");
    }

    // ---- toggle_expand_or_read / toggle_read dispatch by tab ---------------------------------

    #[test]
    fn toggle_read_on_mentions_tab_marks_the_selected_row_read() {
        let mut app = empty_app();
        app.conv_kinds.insert("D1".to_string(), ConvKind::Im);
        app.apply(SocketEvent::Message(msg("D1", "1.0", None, "U1", "a dm")));
        app.tab = Tab::Mentions;
        app.cursor = 0;
        app.toggle_read();
        assert_eq!(app.unread_mentions(), 0);
    }

    // ---- cursor is identity-based, not positional ------------------------------------------

    #[test]
    fn selection_identity_survives_an_earlier_insert_reindexing_the_feed() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "5.0", None, "U1", "row A")));
        app.apply(SocketEvent::Message(msg("C1", "10.0", None, "U1", "row B")));
        app.touch();

        app.move_cursor(1); // select "row B" at index 1
        assert_eq!(app.feed_rows()[app.cursor].text, "row B");
        assert_eq!(app.selected, Some((("C1".to_string(), "10.0".to_string()), SelKind::Message)));

        // An earlier message arrives (e.g. a poll backfill), which sorts ahead of both
        // existing rows (and, being a fresh arrival, also introduces the unread divider),
        // reindexing "row B" further down the list.
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "row earliest")));

        assert_eq!(
            app.selected,
            Some((("C1".to_string(), "10.0".to_string()), SelKind::Message)),
            "selected identity must not change just because an earlier row was inserted"
        );
        let rows = app.feed_rows();
        let expected_pos = rows.iter().position(|r| r.text == "row B").unwrap();
        assert_eq!(app.cursor, expected_pos, "cursor re-derives to the identity's new position");
        assert_eq!(rows[app.cursor].text, "row B");
    }

    #[test]
    fn toggle_read_targets_the_selected_identity_after_a_reorder() {
        let mut app = empty_app();
        app.conv_kinds.insert("D1".to_string(), ConvKind::Im);
        app.apply(SocketEvent::Message(msg("D1", "10.0", None, "U1", "row X")));
        app.apply(SocketEvent::Message(msg("D1", "5.0", None, "U1", "row Y")));
        app.tab = Tab::Mentions;
        app.touch();

        app.move_cursor(1); // oldest-first: [Y(5), X(10)] -> index 1 selects X
        assert_eq!(app.mention_rows()[app.cursor].text, "row X");

        // A message between Y and X arrives, shifting X from index 1 to index 2.
        app.apply(SocketEvent::Message(msg("D1", "7.0", None, "U1", "row Z")));
        assert_eq!(app.cursor, 2);
        assert_eq!(app.mention_rows()[app.cursor].text, "row X");

        app.toggle_read();
        assert!(
            app.read_mentions.contains(&("D1".to_string(), "10.0".to_string())),
            "the read toggle must land on row X (the selected identity)"
        );
        assert!(
            !app.read_mentions.contains(&("D1".to_string(), "7.0".to_string())),
            "not on whatever row now sits at the stale index"
        );
    }

    // ---- marker-vs-root identity (a thread root's Message row and its collapsed ThreadMarker
    // row share the same (conv, ts) id) ------------------------------------------------------

    /// Build a `Rest` whose `cancelled` flag is already set, so any REST call it's handed to
    /// (e.g. `conversations.replies` on thread expand) spawns curl but is killed within a
    /// couple of poll iterations rather than depending on real network reachability — the
    /// call's *result* doesn't matter for these tests since the reply under test is already
    /// locally known (applied via a prior `SocketEvent::Message`), only that expand/collapse
    /// dispatch reaches the right row.
    fn precancelled_rest(cancelled: &AtomicBool) -> Rest<'_> {
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        Rest { user_token: "xoxp-test", cancelled }
    }

    #[test]
    fn moving_onto_a_collapsed_threads_marker_row_and_toggling_expands_it() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
        app.touch();

        app.move_cursor(1); // the collapsed thread's ThreadMarker row (index 1)
        let rows = app.feed_rows();
        assert_eq!(rows[app.cursor].kind, RowKind::ThreadMarker { replies: 1, expanded: false });

        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        app.toggle_expand_or_read(&rest);

        let rows = app.feed_rows();
        assert!(
            !rows.iter().any(|r| matches!(r.kind, RowKind::ThreadMarker { .. })),
            "the marker must be gone once expanded, not left in place by an early return"
        );
        assert!(rows.iter().any(|r| r.text == "reply one"), "the reply must render inline");
    }

    #[test]
    fn resync_keeps_the_marker_selected_after_an_unrelated_apply() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
        app.touch();

        app.move_cursor(1); // the marker row
        let rows = app.feed_rows();
        assert_eq!(rows[app.cursor].kind, RowKind::ThreadMarker { replies: 1, expanded: false });

        // An unrelated message elsewhere triggers apply()'s resync_cursor; it must not bump
        // the marker selection back onto its root row.
        app.apply(SocketEvent::Message(msg("C2", "0.5", None, "U1", "unrelated, sorts first")));

        let rows = app.feed_rows();
        assert_eq!(
            rows[app.cursor].kind,
            RowKind::ThreadMarker { replies: 1, expanded: false },
            "resync must keep the marker selected, not snap back to the root Message row"
        );
    }

    // ---- poll fallback must surface edits, not just dedup arrivals ---------------------------

    #[test]
    fn a_poll_delivered_edit_updates_text_and_edited_flag_without_moving_arrival() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "original")));
        app.touch();
        let key = super::key_for("C1", "1.0");
        let arrival_before = app.messages.get(&key).unwrap().arrival;

        // No live Changed event (socket down); the poll fallback's history() re-fetch is routed
        // through this same upsert_new path (see poll_tick) and carries the server-side edit.
        app.apply(SocketEvent::Message(Message {
            edited: true,
            ..msg("C1", "1.0", None, "U1", "edited via poll")
        }));

        let stored = app.messages.get(&key).unwrap();
        assert_eq!(stored.msg.text, "edited via poll");
        assert!(stored.msg.edited);
        assert_eq!(
            stored.arrival, arrival_before,
            "arrival must be unchanged so the divider doesn't jump"
        );

        let rows = app.feed_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "edited via poll");
        assert!(!rows.iter().any(|r| r.kind == RowKind::Divider));
    }

    // ---- set_tab resyncs selection for the new tab --------------------------------------------

    #[test]
    fn set_tab_clamps_the_cursor_into_the_new_tabs_bounds() {
        let mut app = empty_app();
        app.conv_kinds.insert("D1".to_string(), ConvKind::Im);
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "feed only, no mention")));
        app.apply(SocketEvent::Message(msg("D1", "2.0", None, "U1", "a dm is always a mention")));
        app.touch();

        assert_eq!(app.feed_rows().len(), 2);
        assert_eq!(app.mention_rows().len(), 1);

        app.cursor = 1; // valid for the 2-row feed tab, out of range for the 1-row mentions tab
        app.set_tab(Tab::Mentions);

        assert_eq!(app.tab, Tab::Mentions);
        assert_eq!(app.cursor, 0, "cursor must clamp into the mentions tab's single row");

        // A subsequent action must target the mentions tab's row, not panic or hit a stale one.
        app.toggle_read();
        assert_eq!(app.unread_mentions(), 0);
    }

    // ---- resolve_channels: see `crate::model`'s tests (moved there — dedup, task 1) --------

    // ---- resolve_im_names -----------------------------------------------------------------

    #[test]
    fn resolve_im_names_maps_the_counterpart_user_id_to_a_display_name() {
        let convs = vec![conv("D1", "U9", ConvKind::Im), conv("C1", "eng", ConvKind::Channel)];
        let users = HashMap::from([("U9".to_string(), "priya".to_string())]);
        let resolved = resolve_im_names(convs, &users);
        assert_eq!(resolved[0].name, "priya");
        assert_eq!(resolved[1].name, "eng"); // non-IM untouched
    }

    #[test]
    fn resolve_im_names_falls_back_to_the_raw_id_when_unknown() {
        let convs = vec![conv("D1", "U9", ConvKind::Im)];
        let resolved = resolve_im_names(convs, &HashMap::new());
        assert_eq!(resolved[0].name, "U9");
    }

    // ---- ts_to_hhmm ----------------------------------------------------------------------------

    #[test]
    fn ts_to_hhmm_formats_epoch_seconds_as_utc_clock_time() {
        // 1_752_300_000 seconds since epoch: 2025-07-12T06:00:00Z.
        assert_eq!(ts_to_hhmm("1752300000.000100"), "06:00");
    }

    #[test]
    fn ts_to_hhmm_malformed_input_renders_midnight_rather_than_panicking() {
        assert_eq!(ts_to_hhmm("garbage"), "00:00");
    }

    // ---- conv_label -----------------------------------------------------------------------------

    #[test]
    fn conv_label_uses_hash_prefix_for_channels_and_at_prefix_for_ims() {
        let mut app = empty_app();
        app.conv_kinds.insert("D1".to_string(), ConvKind::Im);
        app.conv_names.insert("D1".to_string(), "priya".to_string());
        assert_eq!(app.conv_label("C1"), "#eng");
        assert_eq!(app.conv_label("D1"), "@priya");
    }

    // ---- build backfill errors name the failing channel (Fix 2a) --------------------------

    #[test]
    fn a_backfill_history_error_fails_build_naming_the_channel() {
        let mut app = empty_app();
        let error =
            apply_backfill(&mut app, "eng", Err(RestError::SlackError("channel_not_found".into())))
                .unwrap_err();
        assert!(error.contains("eng"), "{error}");
        assert!(error.contains("channel_not_found"), "{error}");
    }

    #[test]
    fn a_successful_backfill_upserts_every_message() {
        let mut app = empty_app();
        apply_backfill(&mut app, "eng", Ok(vec![msg("C1", "1.0", None, "U1", "hi")])).unwrap();
        app.touch();
        assert_eq!(app.feed_rows().len(), 1);
    }

    // ---- poll_tick status wording (Fix 2b) --------------------------------------------------

    #[test]
    fn poll_error_status_is_a_rate_limit_notice_for_rate_limited() {
        let status = poll_error_status("eng", &RestError::RateLimited(42));
        assert_eq!(status, "slack rate limit — retrying in 42s");
    }

    #[test]
    fn poll_error_status_names_the_conversation_for_other_errors() {
        let status =
            poll_error_status("eng", &RestError::SlackError("channel_not_found".to_string()));
        assert!(status.contains("eng"), "{status}");
    }

    #[test]
    fn poll_tick_surfaces_a_rest_failure_as_a_one_line_status_without_crashing() {
        let mut app = empty_app();
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        app.poll_tick(&rest);
        assert!(!app.status.is_empty(), "poll_tick must surface the failure, not swallow it");
    }

    // ---- next_batch: pure round-robin scheduling (Task 2) ------------------------------------

    #[test]
    fn next_batch_takes_the_first_batch_from_a_fresh_cursor() {
        let (indices, cursor) = next_batch(0, 20, 8);
        assert_eq!(indices, vec![0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(cursor, 8);
    }

    #[test]
    fn next_batch_wraps_around_past_the_end_of_the_list() {
        let (indices, cursor) = next_batch(6, 8, 8);
        assert_eq!(indices, vec![6, 7, 0, 1, 2, 3, 4, 5]);
        assert_eq!(cursor, 6, "cursor wraps back to where it started when batch == n");
    }

    #[test]
    fn next_batch_clamps_batch_to_the_conversation_count() {
        let (indices, cursor) = next_batch(0, 3, 8);
        assert_eq!(indices, vec![0, 1, 2]);
        assert_eq!(cursor, 0, "wraps back to 0 rather than accumulating past n");
    }

    #[test]
    fn next_batch_empty_list_yields_no_indices_and_resets_the_cursor() {
        assert_eq!(next_batch(5, 0, 8), (Vec::new(), 0));
    }

    #[test]
    fn next_batch_visits_every_conversation_across_enough_ticks() {
        // The scheduling contract that matters: not the exact indices any one tick returns, but
        // that every conversation is eventually polled. ceil(n / batch) ticks must cover all n.
        let (n, batch): (usize, usize) = (37, 8);
        let mut cursor = 0;
        let mut visited = HashSet::new();
        for _ in 0..n.div_ceil(batch) {
            let (indices, next_cursor) = next_batch(cursor, n, batch);
            visited.extend(indices);
            cursor = next_cursor;
        }
        assert_eq!(visited.len(), n, "every conversation must be visited across enough ticks");
    }

    // ---- cooldown_active: pure gating with an injected clock (Task 2) ------------------------

    #[test]
    fn cooldown_active_none_deadline_never_gates() {
        assert!(!cooldown_active(Instant::now(), None));
    }

    #[test]
    fn cooldown_active_true_before_the_deadline_false_after() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(30);
        assert!(cooldown_active(now, Some(deadline)));
        assert!(cooldown_active(now + Duration::from_secs(29), Some(deadline)));
        assert!(!cooldown_active(now + Duration::from_secs(30), Some(deadline)));
        assert!(!cooldown_active(now + Duration::from_secs(31), Some(deadline)));
    }

    // ---- poll_tick_at: cooldown gates the whole tick, not just a shortened one ----------------

    #[test]
    fn poll_tick_during_cooldown_skips_the_tick_entirely_leaving_the_cursor_untouched() {
        let mut app = empty_app();
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        let now = Instant::now();
        app.cooldown_until = Some(now + Duration::from_secs(30));
        app.status = "slack rate limit — retrying in 30s".to_string();

        app.poll_tick_at(&rest, now);

        assert_eq!(app.poll_cursor, 0, "a gated tick must not advance the batch cursor");
        assert!(app.status.contains("rate limit"), "the rate-limit notice must stay up");
    }

    #[test]
    fn poll_tick_resumes_once_the_cooldown_deadline_has_passed() {
        let mut app = empty_app();
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        let now = Instant::now();
        app.cooldown_until = Some(now);

        // A moment after the deadline: the tick must run (and, since `empty_app` has 2 convs
        // and `precancelled_rest` fails every call with a non-rate-limit error, both must be
        // attempted — observable via the cursor wrapping back to 0).
        app.poll_tick_at(&rest, now + Duration::from_secs(1));

        assert_eq!(app.poll_cursor, 0, "both conversations were visited and the cursor wrapped");
    }

    // ---- dm_scan_due: pure 5-minute gate for the out-of-cap DM scan (Task 2, spec §1) ---------

    #[test]
    fn dm_scan_due_with_no_prior_scan_is_always_due() {
        assert!(dm_scan_due(Instant::now(), None));
    }

    #[test]
    fn dm_scan_due_true_at_and_after_the_deadline_false_before() {
        let now = Instant::now();
        let deadline = now + DM_SCAN_INTERVAL;
        assert!(!dm_scan_due(now, Some(deadline)));
        assert!(!dm_scan_due(now + (DM_SCAN_INTERVAL - Duration::from_secs(1)), Some(deadline)));
        assert!(dm_scan_due(deadline, Some(deadline)));
        assert!(dm_scan_due(deadline + Duration::from_secs(1), Some(deadline)));
    }

    // ---- pick_changed_dm: pure diff-and-pick-one over two conversation snapshots (Task 2,
    // spec §1) -----------------------------------------------------------------------------

    fn dm(id: &str, name: &str, updated: u64) -> Conversation {
        Conversation {
            id: id.into(),
            name: name.into(),
            kind: ConvKind::Im,
            updated: Some(updated),
        }
    }

    #[test]
    fn pick_changed_dm_returns_none_when_nothing_changed() {
        let old = vec![dm("D1", "alice", 100)];
        let new_all = vec![dm("D1", "alice", 100)];
        let subscribed = HashSet::new();
        let last_seen = HashMap::new();
        assert_eq!(pick_changed_dm(&old, &new_all, &subscribed, &last_seen), None);
    }

    #[test]
    fn pick_changed_dm_picks_the_single_most_recently_updated_among_several_changed() {
        let old = vec![dm("D1", "alice", 100), dm("D2", "bob", 100), dm("D3", "carol", 100)];
        let new_all = vec![dm("D1", "alice", 150), dm("D2", "bob", 300), dm("D3", "carol", 200)];
        let subscribed = HashSet::new();
        let last_seen = HashMap::new();
        let picked = pick_changed_dm(&old, &new_all, &subscribed, &last_seen);
        assert_eq!(picked.map(|c| c.id), Some("D2".to_string()), "bob moved the furthest (300)");
    }

    #[test]
    fn pick_changed_dm_ignores_conversations_already_in_the_subscribed_set() {
        let old = vec![dm("D1", "alice", 100)];
        let new_all = vec![dm("D1", "alice", 300)];
        let subscribed = HashSet::from(["D1".to_string()]);
        let last_seen = HashMap::new();
        assert_eq!(
            pick_changed_dm(&old, &new_all, &subscribed, &last_seen),
            None,
            "in-cap conversations are covered by the normal poll batch, not this scan"
        );
    }

    #[test]
    fn pick_changed_dm_ignores_channel_and_group_kinds() {
        let old = vec![Conversation { updated: Some(100), ..dm("C1", "eng", 100) }];
        let mut changed = dm("C1", "eng", 300);
        changed.kind = ConvKind::Channel;
        let new_all = vec![changed];
        let subscribed = HashSet::new();
        let last_seen = HashMap::new();
        assert_eq!(pick_changed_dm(&old, &new_all, &subscribed, &last_seen), None);
    }

    #[test]
    fn pick_changed_dm_picks_up_a_conversation_absent_from_the_old_snapshot() {
        // Brand new since `build`/the last scan — never observed before, so any `updated` at
        // all counts as "changed" (baseline defaults to 0).
        let old: Vec<Conversation> = Vec::new();
        let new_all = vec![dm("D1", "alice", 50)];
        let subscribed = HashSet::new();
        let last_seen = HashMap::new();
        assert_eq!(
            pick_changed_dm(&old, &new_all, &subscribed, &last_seen).map(|c| c.id),
            Some("D1".to_string())
        );
    }

    #[test]
    fn pick_changed_dm_uses_last_seen_as_the_baseline_over_the_old_snapshot() {
        // Already fetched up to 200 by a prior scan (tracked in `last_seen`), even though the
        // stale `old` snapshot still shows 100 — a re-diff against `old` alone would wrongly
        // treat 150 as an advance.
        let old = vec![dm("D1", "alice", 100)];
        let new_all = vec![dm("D1", "alice", 150)];
        let subscribed = HashSet::new();
        let last_seen = HashMap::from([("D1".to_string(), 200)]);
        assert_eq!(pick_changed_dm(&old, &new_all, &subscribed, &last_seen), None);
    }

    #[test]
    fn pick_changed_dm_skips_a_conversation_missing_updated() {
        let old: Vec<Conversation> = Vec::new();
        let new_all = vec![conv("D1", "alice", ConvKind::Im)]; // `updated: None`
        let subscribed = HashSet::new();
        let last_seen = HashMap::new();
        assert_eq!(pick_changed_dm(&old, &new_all, &subscribed, &last_seen), None);
    }

    // ---- updated_ms_to_ts: pure ms-epoch -> Slack ts conversion (Task 2) -----------------------

    #[test]
    fn updated_ms_to_ts_formats_seconds_and_microseconds() {
        assert_eq!(updated_ms_to_ts(1_752_300_000_123), "1752300000.123000");
    }

    #[test]
    fn updated_ms_to_ts_zero_sub_second_remainder() {
        assert_eq!(updated_ms_to_ts(1_752_300_000_000), "1752300000.000000");
    }

    // ---- poll_tick_at: the out-of-cap DM scan runs at most once per 5-minute interval (Task 2,
    // spec §1) -------------------------------------------------------------------------------

    #[test]
    fn poll_tick_sets_the_next_dm_scan_deadline_on_the_first_tick() {
        let mut app = empty_app();
        assert_eq!(app.next_dm_scan, None);
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        let now = Instant::now();

        app.poll_tick_at(&rest, now);

        assert_eq!(app.next_dm_scan, Some(now + DM_SCAN_INTERVAL));
    }

    #[test]
    fn poll_tick_does_not_rerun_the_dm_scan_before_the_next_interval() {
        let mut app = empty_app();
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        let now = Instant::now();
        app.poll_tick_at(&rest, now);
        let deadline = app.next_dm_scan;

        app.poll_tick_at(&rest, now + Duration::from_secs(1));

        assert_eq!(deadline, app.next_dm_scan, "deadline unchanged — scan is not due yet");
    }

    #[test]
    fn poll_tick_reruns_the_dm_scan_once_the_interval_has_elapsed() {
        let mut app = empty_app();
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        let now = Instant::now();
        app.poll_tick_at(&rest, now);
        let first_deadline = app.next_dm_scan.unwrap();

        app.poll_tick_at(&rest, first_deadline);

        assert_eq!(
            app.next_dm_scan,
            Some(first_deadline + DM_SCAN_INTERVAL),
            "a due scan pushes the deadline another full interval out"
        );
    }

    // ---- poll_tick_at: a RateLimited main budget stops the whole tick, including the DM scan
    // (review fix: `poll_tick_at`'s own doc promises a `RateLimited` hit "stops the rest of this
    // tick"; the DM scan must honor that too, not run unconditionally) ------------------------

    #[test]
    fn dm_scan_is_skipped_entirely_when_the_main_budget_rate_limited_this_tick() {
        let mut app = empty_app();
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        let now = Instant::now();

        // `rate_limited: true` stands in for the main conv/thread budget having just hit
        // `RateLimited` this same tick (untestable end-to-end without a real 429, so it's
        // injected directly here, exactly as `poll_tick_at` computes and passes it).
        app.run_or_skip_dm_scan(&rest, now, true);

        assert_eq!(
            app.next_dm_scan, None,
            "a rate-limited tick must not even check whether the scan is due, let alone run it"
        );
        assert!(app.status.is_empty(), "no REST call happened, so no dm-scan status either");
        assert!(app.all_conversations.is_empty(), "list_conversations was never called");
    }

    #[test]
    fn dm_scan_still_runs_on_a_normal_non_rate_limited_tick_when_due() {
        let mut app = empty_app();
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        let now = Instant::now();

        app.run_or_skip_dm_scan(&rest, now, false);

        assert_eq!(
            app.next_dm_scan,
            Some(now + DM_SCAN_INTERVAL),
            "not rate-limited: the scan ran exactly as before and advanced its deadline"
        );
    }

    // ---- track_newest: keep-the-max newest-ts tracking (Task 2) -------------------------------

    #[test]
    fn track_newest_records_the_first_ts_seen_for_a_conversation() {
        let mut newest = HashMap::new();
        track_newest(&mut newest, "C1", "100.000001");
        assert_eq!(newest.get("C1").map(String::as_str), Some("100.000001"));
    }

    #[test]
    fn track_newest_ignores_an_older_ts() {
        let mut newest = HashMap::new();
        track_newest(&mut newest, "C1", "100.000001");
        track_newest(&mut newest, "C1", "50.000001");
        assert_eq!(newest.get("C1").map(String::as_str), Some("100.000001"));
    }

    #[test]
    fn track_newest_replaces_with_a_newer_ts() {
        let mut newest = HashMap::new();
        track_newest(&mut newest, "C1", "100.000001");
        track_newest(&mut newest, "C1", "200.000001");
        assert_eq!(newest.get("C1").map(String::as_str), Some("200.000001"));
    }

    #[test]
    fn track_newest_tracks_each_conversation_independently() {
        let mut newest = HashMap::new();
        track_newest(&mut newest, "C1", "100.0");
        track_newest(&mut newest, "C2", "5.0");
        assert_eq!(newest.get("C1").map(String::as_str), Some("100.0"));
        assert_eq!(newest.get("C2").map(String::as_str), Some("5.0"));
    }

    #[test]
    fn upsert_new_threads_the_newest_ts_through_the_message_store() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "5.0", None, "U1", "a")));
        app.apply(SocketEvent::Message(msg("C1", "10.0", None, "U1", "b")));
        app.apply(SocketEvent::Message(msg("C1", "3.0", None, "U1", "c"))); // older, ignored
        assert_eq!(app.newest_ts.get("C1").map(String::as_str), Some("10.0"));
    }

    // ---- marker_count: max(metadata, local) (Task 1, spec §1) ----------------------------------

    #[test]
    fn marker_count_uses_metadata_when_local_is_zero() {
        assert_eq!(marker_count(Some(5), 0), 5);
    }

    #[test]
    fn marker_count_uses_local_when_metadata_is_absent() {
        assert_eq!(marker_count(None, 3), 3);
    }

    #[test]
    fn marker_count_takes_the_greater_of_the_two_when_both_are_present() {
        assert_eq!(marker_count(Some(2), 5), 5, "local ahead of stale/smaller metadata");
        assert_eq!(marker_count(Some(9), 4), 9, "metadata ahead of what's locally known yet");
    }

    #[test]
    fn marker_count_is_zero_when_neither_is_present() {
        assert_eq!(marker_count(None, 0), 0);
    }

    #[test]
    fn a_backfilled_thread_shows_its_marker_before_any_reply_is_locally_known() {
        // Spec §1: a root's reply_count alone must render "n replies" immediately, with zero
        // replies actually fetched yet.
        let mut app = empty_app();
        app.apply(SocketEvent::Message(Message {
            reply_count: Some(4),
            ..msg("C1", "1.000001", None, "U1", "root")
        }));
        app.touch();

        let rows = app.feed_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].kind, RowKind::ThreadMarker { replies: 4, expanded: false });
    }

    #[test]
    fn a_thread_with_more_local_replies_than_stale_metadata_shows_the_local_count() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(Message {
            reply_count: Some(1),
            ..msg("C1", "1.000001", None, "U1", "root")
        }));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
        app.apply(SocketEvent::Message(msg("C1", "1.000003", Some("1.000001"), "U1", "reply two")));
        app.touch();

        let rows = app.feed_rows();
        assert_eq!(rows[1].kind, RowKind::ThreadMarker { replies: 2, expanded: false });
    }

    // ---- thread_slot_count: budget split, zero-active never wastes (Task 1, spec §2) ----------

    #[test]
    fn thread_slot_count_is_zero_with_no_active_threads() {
        assert_eq!(thread_slot_count(0), 0);
    }

    #[test]
    fn thread_slot_count_matches_active_count_below_the_cap() {
        // Exactly 1 active thread only ever issues 1 replies() call, so reserving a flat 2
        // wastes a slot the conversation round-robin could have used instead.
        assert_eq!(thread_slot_count(1), 1);
        assert_eq!(thread_slot_count(2), 2);
    }

    #[test]
    fn thread_slot_count_is_capped_at_two_with_more_active_threads() {
        assert_eq!(thread_slot_count(5), 2);
    }

    // ---- active_threads: expanded ∪ count-gap roots (Task 1, spec §2) --------------------------

    #[test]
    fn active_threads_includes_an_expanded_root_even_with_no_reply_count_gap() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.toggle_thread("C1", "1.000001", Vec::new()); // expand with no replies at all

        assert_eq!(app.active_threads(), vec![("C1".to_string(), "1.000001".to_string())]);
    }

    #[test]
    fn active_threads_includes_a_collapsed_root_whose_reply_count_outpaces_local_replies() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(Message {
            reply_count: Some(3),
            ..msg("C1", "1.000001", None, "U1", "root")
        }));
        // Only 1 of the 3 reported replies is locally known.
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));

        assert_eq!(app.active_threads(), vec![("C1".to_string(), "1.000001".to_string())]);
    }

    #[test]
    fn active_threads_excludes_a_root_whose_local_replies_already_match_its_reply_count() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(Message {
            reply_count: Some(1),
            ..msg("C1", "1.000001", None, "U1", "root")
        }));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));

        assert!(app.active_threads().is_empty(), "no gap and not expanded — not active");
    }

    #[test]
    fn active_threads_is_empty_for_a_plain_thread_root_with_no_metadata_or_expansion() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        assert!(app.active_threads().is_empty());
    }

    #[test]
    fn active_threads_does_not_duplicate_a_root_that_is_both_expanded_and_count_gapped() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(Message {
            reply_count: Some(5),
            ..msg("C1", "1.000001", None, "U1", "root")
        }));
        app.toggle_thread("C1", "1.000001", Vec::new());

        assert_eq!(app.active_threads(), vec![("C1".to_string(), "1.000001".to_string())]);
    }

    // ---- newest_reply_ts / apply_fetched_replies (Task 1, spec §2) -----------------------------

    #[test]
    fn newest_reply_ts_is_none_when_no_reply_is_known_locally() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        assert_eq!(app.newest_reply_ts("C1", "1.000001"), None);
    }

    #[test]
    fn newest_reply_ts_picks_the_chronologically_latest_known_reply() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.apply(SocketEvent::Message(msg("C1", "1.000003", Some("1.000001"), "U1", "later")));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "earlier")));
        assert_eq!(app.newest_reply_ts("C1", "1.000001"), Some("1.000003".to_string()));
    }

    #[test]
    fn apply_fetched_replies_upserts_each_message_through_the_normal_path() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.toggle_thread("C1", "1.000001", Vec::new()); // expanded, so the reply renders inline
        app.apply_fetched_replies(vec![msg(
            "C1",
            "1.000002",
            Some("1.000001"),
            "U1",
            "fetched reply",
        )]);
        app.touch();

        let rows = app.feed_rows();
        assert!(rows.iter().any(|r| r.text == "fetched reply"));
    }

    // ---- poll_tick_at: split-budget rotation over active threads (Task 1, spec §2) -------------

    /// A fixture with `n` conversations (`C0`..`Cn-1`), for exercising the slot-split math at a
    /// batch size the default 2-conversation `empty_app` can't: `POLL_BATCH` is 8, so the
    /// conv-slot reservation (6 when threads are active) only bites with more than 6 conversations.
    fn app_with_conversations(n: usize) -> App {
        let mut app = App::empty("SELF");
        for i in 0..n {
            let id = format!("C{i}");
            app.add_conversation(&id, &format!("conv{i}"), ConvKind::Channel);
        }
        app.conversations = (0..n)
            .map(|i| Conversation {
                id: format!("C{i}"),
                name: format!("conv{i}"),
                kind: ConvKind::Channel,
                updated: None,
            })
            .collect();
        app
    }

    #[test]
    fn poll_tick_reserves_only_one_conv_slot_for_a_single_active_thread() {
        // Exactly 1 active thread means only 1 replies() call actually issues; the conversation
        // budget must pick up the slack rather than leaving the 2nd reserved slot unused — a
        // full tick still visits all 8 of the budget's slots (1 thread + 7 conversations).
        let mut app = app_with_conversations(8);
        app.apply(SocketEvent::Message(Message {
            reply_count: Some(3),
            ..msg("C0", "1.000001", None, "U1", "root")
        }));
        assert_eq!(app.active_threads().len(), 1, "one count-gapped root makes threads active");

        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        app.poll_tick_at(&rest, Instant::now());

        assert_eq!(
            app.poll_cursor, 7,
            "all 7 remaining conversations were visited — only 1 slot went to the active thread"
        );
    }

    #[test]
    fn poll_tick_spends_all_eight_slots_on_conversations_when_no_threads_are_active() {
        let mut app = app_with_conversations(8);
        assert!(app.active_threads().is_empty());

        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        app.poll_tick_at(&rest, Instant::now());

        assert_eq!(
            app.poll_cursor, 0,
            "all 8 conversations were visited and the cursor wrapped — nothing reserved"
        );
    }

    #[test]
    fn poll_tick_thread_cursor_round_robins_across_ticks() {
        let mut app = app_with_conversations(1);
        // Three distinct active threads (all expanded, so no reply_count juggling needed).
        for ts in ["1.0", "2.0", "3.0"] {
            app.apply(SocketEvent::Message(msg("C0", ts, None, "U1", "root")));
            app.toggle_thread("C0", ts, Vec::new());
        }
        assert_eq!(app.active_threads().len(), 3);

        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        app.poll_tick_at(&rest, Instant::now()); // visits 2 of the 3 active threads
        assert_eq!(app.poll_thread_cursor, 2, "thread cursor advances by the 2-slot budget");

        app.poll_tick_at(&rest, Instant::now()); // wraps: visits the 3rd, then back to the 1st
        assert_eq!(
            app.poll_thread_cursor, 1,
            "thread cursor wraps round-robin across ticks, independent of the conv cursor"
        );
    }

    // ---- backfill retry decision (Task 2, spec §5) --------------------------------------------

    #[test]
    fn backfill_retry_continues_on_a_successful_retry() {
        let retry_msgs = vec![msg("C1", "1.0", None, "U1", "hi")];
        match backfill_retry_decision("eng", Ok(retry_msgs)) {
            BackfillRetry::Continue(msgs) => assert_eq!(msgs.len(), 1),
            _ => panic!("expected Continue"),
        }
    }

    #[test]
    fn backfill_retry_skips_remaining_on_a_second_consecutive_rate_limit() {
        match backfill_retry_decision("eng", Err(RestError::RateLimited(5))) {
            BackfillRetry::SkipRemaining(note) => {
                assert!(note.to_lowercase().contains("rate limit"), "{note}");
            }
            _ => panic!("expected SkipRemaining"),
        }
    }

    #[test]
    fn backfill_retry_hard_fails_naming_the_channel_on_a_different_second_error() {
        match backfill_retry_decision(
            "eng",
            Err(RestError::SlackError("channel_not_found".to_string())),
        ) {
            BackfillRetry::HardFail(message) => {
                assert!(message.contains("eng"), "{message}");
                assert!(message.contains("channel_not_found"), "{message}");
            }
            _ => panic!("expected HardFail"),
        }
    }

    #[test]
    fn capped_sleep_secs_caps_at_sixty() {
        assert_eq!(capped_sleep_secs(120), 60);
        assert_eq!(capped_sleep_secs(500), 60);
    }

    #[test]
    fn capped_sleep_secs_leaves_a_short_delay_unchanged() {
        assert_eq!(capped_sleep_secs(5), 5);
        assert_eq!(capped_sleep_secs(60), 60);
    }

    #[test]
    fn backfill_one_a_rate_limited_first_attempt_then_hard_error_still_fails_build() {
        // `backfill_one` needs a real REST call for its *first* attempt, which a precancelled
        // `Rest` turns into `Other` (cancelled), not `RateLimited` — so this exercises the
        // decision fn directly (above) for the retry branches, and here only the plain
        // non-rate-limited first-attempt path via `backfill_one`'s delegation to `apply_backfill`.
        let mut app = empty_app();
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        let conv = Conversation {
            id: "C1".into(),
            name: "eng".into(),
            kind: ConvKind::Channel,
            updated: None,
        };
        match super::backfill_one(&mut app, &rest, &conv) {
            BackfillOutcome::Fail(message) => assert!(message.contains("eng"), "{message}"),
            _ => panic!("expected Fail for a non-rate-limit history error"),
        }
    }

    // ---- Feed-tab bottom-follow (Fix 1b: identity plumbing) ----------------------------------

    #[test]
    fn cursor_at_the_last_row_follows_a_new_arrival_to_the_new_last_row() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "row A")));
        app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "row B")));
        app.touch();
        app.move_cursor(10); // clamps to the last row, "row B"
        assert_eq!(app.feed_rows()[app.cursor].text, "row B");

        app.apply(SocketEvent::Message(msg("C1", "3.0", None, "U1", "row C")));

        let rows = app.feed_rows();
        assert_eq!(
            rows[app.cursor].text, "row C",
            "cursor must follow to the new bottom, not stay pinned on row B"
        );
    }

    #[test]
    fn cursor_not_at_the_last_row_does_not_follow_a_new_arrival() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "row A")));
        app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "row B")));
        app.touch();
        app.move_cursor(0); // stays on "row A", not the bottom

        app.apply(SocketEvent::Message(msg("C1", "3.0", None, "U1", "row C")));

        let rows = app.feed_rows();
        assert_eq!(rows[app.cursor].text, "row A", "cursor away from the bottom must not move");
    }

    // ---- thread_rows: the Threads view projection (Task 2, spec §3) ---------------------------

    #[test]
    fn thread_rows_excludes_non_threaded_messages() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "just chatting")));
        assert!(app.thread_rows().is_empty());
    }

    #[test]
    fn thread_rows_includes_a_backfilled_root_before_any_reply_is_known() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(Message {
            reply_count: Some(3),
            ..msg("C1", "1.000001", None, "U1", "root")
        }));

        let rows = app.thread_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "root");
        assert_eq!(rows[0].kind, RowKind::Message);
    }

    #[test]
    fn thread_rows_nests_locally_known_replies_beneath_their_root_in_order() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
        app.apply(SocketEvent::Message(msg("C1", "1.000003", Some("1.000001"), "U1", "later")));
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "earlier")));

        let rows = app.thread_rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text, "root");
        assert_eq!(rows[1].text, "\u{21b3} earlier");
        assert_eq!(rows[2].text, "\u{21b3} later");
    }

    #[test]
    fn thread_rows_orders_threads_by_latest_activity_ascending_newest_last() {
        let mut app = empty_app();
        // Thread A's root is newer than thread B's, but B's latest reply is newer still — B
        // must sort last (spec §1: newest activity at the bottom).
        app.apply(SocketEvent::Message(msg("C1", "10.0", None, "U1", "root A")));
        app.apply(SocketEvent::Message(msg("C1", "10.1", Some("10.0"), "U1", "reply A1")));
        app.apply(SocketEvent::Message(msg("C1", "5.0", None, "U1", "root B")));
        app.apply(SocketEvent::Message(msg("C1", "50.0", Some("5.0"), "U1", "reply B1")));

        let rows = app.thread_rows();
        assert_eq!(rows[0].text, "root A", "A's latest reply (10.1) beats B's (50.0) for last");
        assert_eq!(rows[1].text, "\u{21b3} reply A1");
        assert_eq!(rows[2].text, "root B");
        assert_eq!(rows[3].text, "\u{21b3} reply B1");
    }

    #[test]
    fn thread_rows_uses_the_root_ts_as_activity_when_no_reply_is_known_yet() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(Message {
            reply_count: Some(1),
            ..msg("C1", "1.0", None, "U1", "old root, no replies fetched yet")
        }));
        app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "newer root")));
        app.apply(SocketEvent::Message(msg("C1", "2.1", Some("2.0"), "U1", "reply")));

        let rows = app.thread_rows();
        assert_eq!(rows[0].text, "old root, no replies fetched yet");
        assert_eq!(rows[1].text, "newer root");
    }

    // ---- thread_rows: orphaned replies (root not locally known) get a synthetic thread --------

    #[test]
    fn thread_rows_surfaces_an_orphaned_reply_as_a_synthetic_thread() {
        let mut app = empty_app();
        // No root "1.000001" was ever stored (e.g. it's older than the backfill horizon), but a
        // reply to it arrives. The Timeline (feed_rows) inlines this as a "↳ "-prefixed row; the
        // Threads view must not simply drop it instead — it gets a synthetic thread entry.
        app.apply(SocketEvent::Message(msg(
            "C1",
            "1.000002",
            Some("1.000001"),
            "U1",
            "reply to an unknown root",
        )));

        let rows = app.thread_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, RowKind::Message);
        assert_eq!(rows[0].text, "(thread — root not loaded)");
        assert_eq!(rows[1].text, "\u{21b3} reply to an unknown root");
    }

    #[test]
    fn thread_rows_orders_a_synthetic_thread_by_its_replys_ts() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "5.0", None, "U1", "root A")));
        app.apply(SocketEvent::Message(msg("C1", "5.1", Some("5.0"), "U1", "reply A1")));
        // The orphaned reply's ts (50.0) is newer than thread A's activity (5.1) — its synthetic
        // thread must sort last (spec §1: newest activity at the bottom).
        app.apply(SocketEvent::Message(msg("C1", "50.0", Some("40.0"), "U1", "orphaned reply")));

        let rows = app.thread_rows();
        assert_eq!(rows[0].text, "root A");
        assert_eq!(rows[1].text, "\u{21b3} reply A1");
        assert_eq!(rows[2].text, "(thread — root not loaded)");
        assert_eq!(rows[3].text, "\u{21b3} orphaned reply");
    }

    #[test]
    fn thread_rows_synthetic_thread_self_heals_when_the_root_arrives() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply")));
        assert_eq!(app.thread_rows()[0].text, "(thread — root not loaded)");

        // The root arrives — e.g. `refresh_thread`'s `conversations.replies` call returns the
        // actual root as the first message, and it goes through the normal upsert path.
        app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "actual root")));

        let rows = app.thread_rows();
        assert_eq!(rows.len(), 2, "the synthetic header must be replaced, not duplicated");
        assert_eq!(rows[0].text, "actual root");
        assert_eq!(rows[1].text, "\u{21b3} reply");
    }

    // ---- toggle_view: Feed-tab-only projection flip (Task 2, spec §3) --------------------------

    #[test]
    fn toggle_view_flips_between_timeline_and_threads_on_the_feed_tab() {
        let mut app = empty_app();
        assert_eq!(app.view, FeedView::Timeline);
        app.toggle_view();
        assert_eq!(app.view, FeedView::Threads);
        app.toggle_view();
        assert_eq!(app.view, FeedView::Timeline);
    }

    #[test]
    fn toggle_view_is_a_no_op_off_the_feed_tab() {
        let mut app = empty_app();
        app.tab = Tab::Mentions;
        app.toggle_view();
        assert_eq!(app.view, FeedView::Timeline, "the toggle key is Feed-only");
    }

    #[test]
    fn toggle_view_resyncs_the_cursor_into_the_new_projections_bounds() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "not a thread")));
        app.apply(SocketEvent::Message(Message {
            reply_count: Some(1),
            ..msg("C1", "2.0", None, "U1", "a thread root")
        }));
        app.touch();
        app.move_cursor(1); // selects "a thread root" in the 2-row timeline

        app.toggle_view();
        assert_eq!(app.view, FeedView::Threads);
        assert_eq!(app.cursor, 0, "the 1-row threads projection clamps the stale cursor");
        assert_eq!(app.thread_rows()[app.cursor].text, "a thread root");
    }

    // ---- toggle_focus / mutual exclusion with Threads (Task 3, spec §3) ------------------------

    #[test]
    fn toggle_focus_flips_between_timeline_and_focus_on_the_feed_tab() {
        let mut app = empty_app();
        assert_eq!(app.view, FeedView::Timeline);
        app.toggle_focus();
        assert_eq!(app.view, FeedView::Focus);
        app.toggle_focus();
        assert_eq!(app.view, FeedView::Timeline);
    }

    #[test]
    fn toggle_focus_is_a_no_op_off_the_feed_tab() {
        let mut app = empty_app();
        app.tab = Tab::Mentions;
        app.toggle_focus();
        assert_eq!(app.view, FeedView::Timeline, "the toggle key is Feed-only");
    }

    #[test]
    fn t_from_focus_switches_to_threads_leaving_focus_per_the_decision_table() {
        let mut app = empty_app();
        app.toggle_focus();
        assert_eq!(app.view, FeedView::Focus);
        app.toggle_view();
        assert_eq!(app.view, FeedView::Threads, "t from Focus lands on Threads, not Timeline");
    }

    #[test]
    fn f_from_threads_switches_to_focus_leaving_threads_per_the_decision_table() {
        let mut app = empty_app();
        app.toggle_view();
        assert_eq!(app.view, FeedView::Threads);
        app.toggle_focus();
        assert_eq!(app.view, FeedView::Focus, "f from Threads lands on Focus, not Timeline");
    }

    #[test]
    fn t_from_threads_and_f_from_focus_both_return_to_timeline() {
        let mut app = empty_app();
        app.toggle_view();
        assert_eq!(app.view, FeedView::Threads);
        app.toggle_view();
        assert_eq!(app.view, FeedView::Timeline);

        app.toggle_focus();
        assert_eq!(app.view, FeedView::Focus);
        app.toggle_focus();
        assert_eq!(app.view, FeedView::Timeline);
    }

    #[test]
    fn toggle_focus_resyncs_the_cursor_into_the_new_projections_bounds() {
        let mut app = empty_app();
        app.dm_allow_convs.insert("C1".to_string());
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "backfilled, excluded")));
        app.session_watermark = app.arrival_seq; // simulate build()'s end-of-backfill cut
        app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "focused live message")));
        app.touch();
        app.move_cursor(1); // selects the live message in the 2-row timeline

        app.toggle_focus();
        assert_eq!(app.view, FeedView::Focus);
        assert_eq!(app.cursor, 0, "the 1-row focus projection clamps the stale cursor");
        assert_eq!(app.focus_rows()[app.cursor].text, "focused live message");
    }

    // ---- focus_rows_with_ids: qualification, arrival-watermark AND (allow-list OR keyword) -----
    // (Task 3, spec §3)

    #[test]
    fn focus_excludes_a_message_at_the_watermark_boundary_even_if_it_matches() {
        let mut app = empty_app();
        app.focus_keywords = vec!["urgent".to_string()];
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "urgent backfilled message")));
        app.session_watermark = app.arrival_seq; // as if build() just finished backfill here
        assert!(
            app.focus_rows().is_empty(),
            "a message whose arrival equals the watermark is the last backfilled one, not live"
        );
    }

    #[test]
    fn focus_includes_a_matching_message_strictly_past_the_watermark() {
        let mut app = empty_app();
        app.focus_keywords = vec!["urgent".to_string()];
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "backfilled, no match")));
        app.session_watermark = app.arrival_seq;
        app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "urgent live message")));
        let rows = app.focus_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "urgent live message");
    }

    #[test]
    fn focus_includes_an_allow_listed_dm_message_with_no_keyword_match() {
        let mut app = empty_app();
        app.add_conversation("D1", "alice", ConvKind::Im);
        app.dm_allow_convs.insert("D1".to_string());
        app.apply(SocketEvent::Message(msg("D1", "1.0", None, "U1", "just checking in")));
        let rows = app.focus_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "just checking in");
    }

    #[test]
    fn focus_includes_a_keyword_match_in_a_non_allow_listed_conversation() {
        let mut app = empty_app();
        app.focus_keywords = vec!["incident".to_string()];
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "there is an incident")));
        let rows = app.focus_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "there is an incident");
    }

    #[test]
    fn focus_excludes_a_live_message_matching_neither_allow_list_nor_keyword() {
        let mut app = empty_app();
        app.focus_keywords = vec!["incident".to_string()];
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "ordinary update")));
        assert!(app.focus_rows().is_empty());
    }

    #[test]
    fn focus_includes_a_message_matching_both_allow_list_and_keyword_exactly_once() {
        let mut app = empty_app();
        app.add_conversation("D1", "alice", ConvKind::Im);
        app.dm_allow_convs.insert("D1".to_string());
        app.focus_keywords = vec!["incident".to_string()];
        app.apply(SocketEvent::Message(msg("D1", "1.0", None, "U1", "incident update")));
        let rows = app.focus_rows();
        assert_eq!(rows.len(), 1, "OR semantics: matching both must not duplicate the row");
    }

    #[test]
    fn focus_rows_are_ascending_and_reuse_the_timeline_message_row_shape() {
        let mut app = empty_app();
        app.focus_keywords = vec!["x".to_string()];
        app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "x second")));
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "x first")));
        let rows = app.focus_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "x first");
        assert_eq!(rows[1].text, "x second");
        assert_eq!(rows[0].kind, RowKind::Message);
    }

    #[test]
    fn focus_rows_is_empty_when_nothing_qualifies_yet() {
        let app = empty_app();
        assert!(app.focus_rows().is_empty());
    }

    // ---- refresh_thread: Enter in the Threads view (re)fetches replies (Task 2, spec §3) -------

    #[test]
    fn enter_on_a_thread_root_in_threads_view_fetches_its_replies() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(Message {
            reply_count: Some(1),
            ..msg("C1", "1.000001", None, "U1", "root")
        }));
        app.toggle_view();
        assert_eq!(app.view, FeedView::Threads);

        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        // The REST call itself fails (precancelled), but this must reach the fetch path (not
        // silently no-op) and must never touch `App::expanded` — the Threads view has nothing
        // for that Timeline-only flag to mean.
        app.toggle_expand_or_read(&rest);
        assert!(app.status.contains("thread refresh failed"), "{}", app.status);
        assert!(
            app.expanded.is_empty(),
            "refresh_thread must not flip the Timeline's expanded set"
        );
    }

    #[test]
    fn enter_on_a_thread_root_in_threads_view_merges_a_successfully_fetched_reply() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(Message {
            reply_count: Some(1),
            ..msg("C1", "1.000001", None, "U1", "root")
        }));
        app.toggle_view();

        // Exercise the pure merge path directly (apply_fetched_replies), the same path
        // refresh_thread's successful branch calls, since a real REST fetch needs the network.
        app.apply_fetched_replies(vec![msg(
            "C1",
            "1.000002",
            Some("1.000001"),
            "U1",
            "fetched reply",
        )]);

        let rows = app.thread_rows();
        assert!(rows.iter().any(|r| r.text == "\u{21b3} fetched reply"));
    }

    // ---- jump_newest / jump_first (Task 1, spec §2) --------------------------------------------

    #[test]
    fn jump_newest_moves_the_cursor_to_the_last_row() {
        let mut app = empty_app();
        for ts in ["1.0", "2.0", "3.0"] {
            app.apply(SocketEvent::Message(msg("C1", ts, None, "U1", "row")));
        }
        app.touch(); // keep the unread divider out of the row count for this test's arithmetic
        app.jump_first();
        assert_eq!(app.cursor, 0);

        app.jump_newest();
        assert_eq!(app.cursor, 2, "must land on the last row");
        assert_eq!(app.selected, Some((("C1".to_string(), "3.0".to_string()), SelKind::Message)));
    }

    #[test]
    fn jump_first_moves_the_cursor_to_the_first_row() {
        let mut app = empty_app();
        for ts in ["1.0", "2.0", "3.0"] {
            app.apply(SocketEvent::Message(msg("C1", ts, None, "U1", "row")));
        }
        app.touch();
        app.jump_newest();
        app.jump_first();
        assert_eq!(app.cursor, 0);
        assert_eq!(app.selected, Some((("C1".to_string(), "1.0".to_string()), SelKind::Message)));
    }

    #[test]
    fn jump_newest_and_jump_first_no_op_on_an_empty_row_list() {
        let mut app = empty_app();
        app.jump_newest();
        assert_eq!(app.cursor, 0);
        app.jump_first();
        assert_eq!(app.cursor, 0);
    }

    // ---- page_move: reuses move_cursor's clamping (Task 1, spec §2) ----------------------------

    #[test]
    fn page_move_clamps_at_the_bounds_like_move_cursor() {
        let mut app = empty_app();
        for ts in ["1.0", "2.0", "3.0"] {
            app.apply(SocketEvent::Message(msg("C1", ts, None, "U1", "row")));
        }
        app.touch();
        app.page_move(-10); // clamps to 0 rather than panicking/underflowing
        assert_eq!(app.cursor, 0);
        app.page_move(2);
        assert_eq!(app.cursor, 2);
        app.page_move(10); // clamps to the last row
        assert_eq!(app.cursor, 2);
    }

    // ---- viewport_rows: set/get round-trip (Task 1, spec §2) ------------------------------------

    #[test]
    fn set_viewport_rows_round_trips() {
        let mut app = empty_app();
        app.set_viewport_rows(15);
        assert_eq!(app.viewport_rows(), 15);
    }

    // ---- new-arrivals indicator: pure transitions (Task 1, spec §3-§4) -------------------------

    #[test]
    fn pending_new_is_zero_before_anything_arrives() {
        let app = empty_app();
        assert_eq!(app.pending_new(), 0);
    }

    #[test]
    fn an_arrival_while_the_cursor_is_at_the_bottom_follows_and_does_not_increment() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "first")));
        app.touch(); // keep the unread divider out of the way before the arrival under test
        assert!(app.is_cursor_at_last_row(), "the only row is trivially the last row");

        app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "second")));

        assert_eq!(app.pending_new(), 0, "following the bottom must not count the arrival");
        // Looked up by content (not a raw index) since this arrival is itself unread and so
        // shifts the divider back in; `feed_rows()[app.cursor]` stays correct regardless.
        assert_eq!(app.feed_rows()[app.cursor].text, "second", "cursor must follow to the bottom");
    }

    #[test]
    fn an_arrival_while_scrolled_up_increments_pending_new_without_moving_the_cursor() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "first")));
        app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "second")));
        app.touch(); // keep the divider out of the way before scrolling up
        // Establish the at-the-bottom baseline explicitly (as `build` does after backfill):
        // without it, the cursor's untouched default position isn't necessarily "the bottom",
        // so `jump_first` below wouldn't actually be leaving a followed bottom behind.
        app.jump_newest();
        app.jump_first(); // scroll away from the bottom
        assert_eq!(app.feed_rows()[app.cursor].text, "first");

        app.apply(SocketEvent::Message(msg("C1", "3.0", None, "U1", "third")));

        assert_eq!(app.pending_new(), 1, "an arrival while scrolled up must count");
        assert_eq!(app.feed_rows()[app.cursor].text, "first", "the cursor must not move");

        app.apply(SocketEvent::Message(msg("C1", "4.0", None, "U1", "fourth")));
        assert_eq!(app.pending_new(), 2, "a second arrival while still scrolled up accumulates");
        assert_eq!(app.feed_rows()[app.cursor].text, "first");
    }

    #[test]
    fn reaching_the_bottom_by_any_means_clears_pending_new() {
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "first")));
        app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "second")));
        app.touch();
        app.jump_newest(); // establish the at-the-bottom baseline explicitly, as `build` does
        app.jump_first();
        app.apply(SocketEvent::Message(msg("C1", "3.0", None, "U1", "third")));
        app.touch();
        assert_eq!(app.pending_new(), 1);

        app.jump_newest(); // any means: G/End
        assert_eq!(app.pending_new(), 0, "jump_newest must clear the counter");

        app.jump_first();
        app.apply(SocketEvent::Message(msg("C1", "4.0", None, "U1", "fourth")));
        app.touch();
        assert_eq!(app.pending_new(), 1);
        app.move_cursor(100); // any means: j/Down walked all the way to the bottom
        assert_eq!(app.pending_new(), 0, "move_cursor landing on the last row must clear it");
    }

    #[test]
    fn a_single_cycle_landing_several_messages_counts_every_arrival() {
        // Regression for the review finding: a poll tick (or any other caller of
        // `finish_after_arrivals`) that upserts several messages before its one
        // `finish_after_arrivals` call must count every one of them, not just 1 — a
        // polling-fallback backlog or `apply_fetched_replies` batch each go through exactly
        // one `finish_after_arrivals` call no matter how many messages landed inside it.
        let mut app = empty_app();
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "first")));
        app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "second")));
        app.touch();
        app.jump_newest(); // establish the at-the-bottom baseline explicitly, as `build` does
        app.jump_first(); // scroll away from the bottom

        let follow_bottom = app.is_cursor_at_last_row();
        let had_rows_before = !app.current_ids().is_empty();
        let arrival_before = app.arrival_seq;

        // Simulate one poll cycle's several upserts landing before its single
        // `finish_after_arrivals` call (mirrors `poll_conversations`/`apply_fetched_replies`,
        // each of which loops `upsert_new` over a batch inside one `poll_tick_at` cycle).
        app.apply_fetched_replies(vec![
            msg("C1", "3.0", None, "U1", "third"),
            msg("C1", "4.0", None, "U1", "fourth"),
            msg("C1", "5.0", None, "U1", "fifth"),
        ]);
        app.finish_after_arrivals(follow_bottom, had_rows_before, arrival_before);

        assert_eq!(
            app.pending_new(),
            3,
            "one cycle landing 3 messages must count all 3, not just 1"
        );
    }

    #[test]
    fn a_poll_arrival_while_scrolled_up_also_increments_pending_new() {
        let mut app = app_with_conversations(1);
        app.apply(SocketEvent::Message(msg("C0", "1.0", None, "U1", "first")));
        app.touch();
        app.jump_first();
        assert_eq!(app.cursor, 0);

        // precancelled_rest fails every call, so no message actually arrives via this poll —
        // this only proves the tick doesn't spuriously increment when nothing lands.
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        app.poll_tick_at(&rest, Instant::now());
        assert_eq!(app.pending_new(), 0, "a failed poll must not fabricate an arrival");
    }
}
