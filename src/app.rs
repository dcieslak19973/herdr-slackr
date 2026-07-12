//! Feed and Mentions state: the in-memory model the pane (Task 7) renders. See
//! `docs/superpowers/specs/2026-07-12-herdr-slackr-design.md` §The pane and §Testing.
//!
//! [`App`] owns every subscribed conversation's messages in one `BTreeMap` keyed by a
//! sortable `(seconds, sequence, conv id)` tuple (`crate::model::ts_key` plus the conv id as
//! a tiebreaker, since two conversations can in principle share a `ts`), so iterating the map
//! in key order *is* the Feed tab's chronological, cross-conversation order for free.
//!
//! [`App::build`] is the only I/O edge: it resolves configured channel names to ids, fetches
//! the self user id and the workspace's user list, and backfills each subscribed
//! conversation's last 50 messages, all via [`crate::rest`]. Everything it delegates to —
//! [`resolve_channels`], [`resolve_im_names`], and the `apply`/row-building/mention/divider
//! logic below — is pure and unit-tested without touching the network (house pattern: see
//! the REST and socket modules' thin-edge-over-pure-core split).
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::config::PluginConfig;
use crate::entities::{self, is_mention};
use crate::model::{ConvKind, Conversation, Message, ts_cmp, ts_key};
use crate::rest::Rest;
use crate::socket::SocketEvent;
use crate::tokens::Tokens;

/// Which tab is active; also which `*_rows`/read semantics `toggle_expand_or_read` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Feed,
    Mentions,
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
/// forbidden) case of two conversations sharing a `ts`.
type Key = (u64, u32, String);

fn key_for(conv: &str, ts: &str) -> Key {
    let (secs, seq) = ts_key(ts);
    (secs, seq, conv.to_string())
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
    /// Index into `visible_rows()` of the active tab (`feed_rows()` for `Tab::Feed`,
    /// `mention_rows()` for `Tab::Mentions`).
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
    self_id: String,
    keywords: Vec<String>,
    /// Monotonic counter, incremented once per newly-seen message (see [`Stored::arrival`]).
    arrival_seq: u64,
    /// The `arrival_seq` value as of the last `touch()`; the divider sits before the first
    /// row whose arrival is greater than this.
    divider_mark: u64,
}

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
        let selected = resolve_channels(config.channels(), config.dms(), &all_convs)?;

        let self_id =
            crate::rest::auth_self(rest).map_err(|e| format!("auth_self failed: {e:?}"))?;
        let users = crate::rest::users(rest).map_err(|e| format!("users failed: {e:?}"))?;
        let user_names: HashMap<String, String> = users.into_iter().collect();
        let selected = resolve_im_names(selected, &user_names);

        let conv_names = selected.iter().map(|c| (c.id.clone(), c.name.clone())).collect();
        let conv_kinds = selected.iter().map(|c| (c.id.clone(), c.kind)).collect();

        let mut app = App {
            tab: Tab::Feed,
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
            self_id,
            keywords: config.keywords().to_vec(),
            arrival_seq: 0,
            divider_mark: 0,
        };

        for conv in app.conversations.clone() {
            if let Ok(msgs) = crate::rest::history(rest, &conv.id, 50) {
                for msg in msgs {
                    app.upsert_new(msg);
                }
            }
        }
        // Everything just backfilled counts as already-seen: the divider only ever marks
        // messages that arrive after this point.
        app.touch();

        Ok(app)
    }

    /// Apply one socket event: insert a new message, replace an edited one in place, remove
    /// a deleted one, or update connection/status state. Mention detection is not done here
    /// — it is recomputed on demand by `mention_rows`/`unread_mentions` from the raw message,
    /// so a later config change (e.g. a new keyword) would not require replaying history.
    pub fn apply(&mut self, ev: SocketEvent) {
        match ev {
            SocketEvent::Connected => {
                self.polling = false;
                self.status.clear();
            }
            SocketEvent::Down(reason) => {
                self.polling = true;
                self.status = format!("socket unavailable ({reason}) — polling");
            }
            SocketEvent::Message(msg) => self.upsert_new(msg),
            SocketEvent::Changed(msg) => self.upsert_edit(msg),
            SocketEvent::Deleted { conv, ts } => self.remove(&conv, &ts),
        }
    }

    /// Fallback mode: re-pull the last 50 messages of every subscribed conversation.
    /// Messages already known (same `(conv, ts)`) are deduplicated by `upsert_new`, so a
    /// message that arrives via both a poll and the socket still appears exactly once.
    pub fn poll_tick(&mut self, rest: &Rest) {
        let convs: Vec<String> = self.conversations.iter().map(|c| c.id.clone()).collect();
        for conv in convs {
            if let Ok(msgs) = crate::rest::history(rest, &conv, 50) {
                for msg in msgs {
                    self.upsert_new(msg);
                }
            }
        }
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
            if stored.msg.thread_ts.is_some() {
                continue; // a reply; rendered below, attached to its root.
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
            if replies.is_empty() {
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
                        text: format!("\u{21b3} {} replies", replies.len()),
                        kind: RowKind::ThreadMarker { replies: replies.len(), expanded: false },
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

    /// The Mentions tab: every message that triggers attention, newest first, each carrying
    /// its read/unread state.
    pub fn mention_rows(&self) -> Vec<Row> {
        self.mention_rows_with_ids().into_iter().map(|(_, row)| row).collect()
    }

    /// As `mention_rows`, but each row is paired with the `(conv, ts)` a Mentions-tab action
    /// (read toggle, permalink) should act on.
    fn mention_rows_with_ids(&self) -> Vec<((String, String), Row)> {
        let mut items: Vec<&Stored> =
            self.messages.values().filter(|s| self.is_mention_stored(s)).collect();
        items.sort_by(|a, b| ts_cmp(&b.msg.ts, &a.msg.ts)); // newest first
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

    /// `Enter` semantics per tab: on the Feed tab, expand/collapse the selected thread
    /// (fetching replies via REST on first expand); on the Mentions tab, toggle the selected
    /// row's read state.
    pub fn toggle_expand_or_read(&mut self, rest: &Rest) {
        match self.tab {
            Tab::Feed => self.toggle_expand(rest),
            Tab::Mentions => self.toggle_read(),
        }
    }

    /// A permalink for the selected row's message, if any (e.g. the cursor is past the end,
    /// or sits on the synthetic `Divider` row).
    pub fn permalink_of_selected(&self, rest: &Rest) -> Option<String> {
        let id = match self.tab {
            Tab::Feed => {
                self.feed_rows_with_ids().into_iter().nth(self.cursor).and_then(|(id, _)| id)
            }
            Tab::Mentions => {
                self.mention_rows_with_ids().into_iter().nth(self.cursor).map(|(id, _)| id)
            }
        }?;
        crate::rest::permalink(rest, &id.0, &id.1).ok()
    }

    // ---- Feed tab: thread expand (thin REST edge over the pure `toggle_thread`) ----------

    fn toggle_expand(&mut self, rest: &Rest) {
        let Some((Some((conv, root_ts)), row)) =
            self.feed_rows_with_ids().into_iter().nth(self.cursor)
        else {
            return;
        };
        if !matches!(row.kind, RowKind::ThreadMarker { .. }) {
            return;
        }
        let will_expand = !self.expanded.contains(&(conv.clone(), root_ts.clone()));
        let fetched = if will_expand {
            crate::rest::replies(rest, &conv, &root_ts).unwrap_or_default()
        } else {
            Vec::new()
        };
        self.toggle_thread(&conv, &root_ts, fetched);
    }

    /// Pure core of thread expand/collapse: flips the expanded flag for `(conv, root_ts)`,
    /// merging `fetched` replies into the store when expanding (collapsing needs no fetch, so
    /// callers pass an empty vec). Returns whether the thread is now expanded. Exposed to
    /// tests so expand/collapse behavior is checked without a real REST call.
    fn toggle_thread(&mut self, conv: &str, root_ts: &str, fetched: Vec<Message>) -> bool {
        let key = (conv.to_string(), root_ts.to_string());
        if self.expanded.remove(&key) {
            false
        } else {
            self.expanded.insert(key);
            for msg in fetched {
                self.upsert_new(msg);
            }
            true
        }
    }

    // ---- Mentions tab: read toggle ------------------------------------------------------

    fn toggle_read(&mut self) {
        let Some((id, _)) = self.mention_rows_with_ids().into_iter().nth(self.cursor) else {
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

    /// Insert a newly-seen message, or refresh an already-known one's fields in place without
    /// disturbing its original arrival order — this is what makes a message arriving via both
    /// a poll and the socket collapse to one entry (`(conv, ts)` is the key either way).
    fn upsert_new(&mut self, msg: Message) {
        let key = key_for(&msg.conv, &msg.ts);
        if let Some(stored) = self.messages.get_mut(&key) {
            stored.msg = msg;
            return;
        }
        self.arrival_seq += 1;
        let arrival = self.arrival_seq;
        self.messages.insert(key, Stored { msg, arrival });
    }

    /// Replace an edited message's fields in place; if it was never seen before (e.g. its
    /// original arrival predates this session), insert it fresh instead of dropping the edit.
    fn upsert_edit(&mut self, msg: Message) {
        let key = key_for(&msg.conv, &msg.ts);
        if let Some(stored) = self.messages.get_mut(&key) {
            stored.msg = msg;
        } else {
            self.arrival_seq += 1;
            let arrival = self.arrival_seq;
            self.messages.insert(key, Stored { msg, arrival });
        }
    }

    fn remove(&mut self, conv: &str, ts: &str) {
        self.messages.remove(&key_for(conv, ts));
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

/// Resolve configured channel names (e.g. `"#eng-infra"`) to their `Conversation`s among
/// `all`, matching only `Channel`/`Group` kinds; an unresolved name is an error naming it
/// (per the design doc: "A configured channel name that resolves to nothing is an error
/// naming the channel"). When `dms` is true, every `Im`/`Mpim` conversation in `all` is
/// included wholesale — DMs are subscribed as a class, not named individually in config.
fn resolve_channels(
    config_channels: &[String],
    dms: bool,
    all: &[Conversation],
) -> Result<Vec<Conversation>, String> {
    let mut out = Vec::with_capacity(config_channels.len());
    for wanted in config_channels {
        let name = wanted.strip_prefix('#').unwrap_or(wanted.as_str());
        let found = all
            .iter()
            .find(|c| c.name == name && matches!(c.kind, ConvKind::Channel | ConvKind::Group));
        match found {
            Some(c) => out.push(c.clone()),
            None => return Err(format!("unknown channel: {wanted}")),
        }
    }
    if dms {
        out.extend(all.iter().filter(|c| matches!(c.kind, ConvKind::Im | ConvKind::Mpim)).cloned());
    }
    Ok(out)
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
    use super::{App, RowKind, Tab, resolve_channels, resolve_im_names, ts_to_hhmm};
    use crate::model::{ConvKind, Conversation, Message};
    use crate::socket::SocketEvent;
    use std::collections::{BTreeMap, HashMap, HashSet};

    fn conv(id: &str, name: &str, kind: ConvKind) -> Conversation {
        Conversation { id: id.into(), name: name.into(), kind }
    }

    fn msg(conv: &str, ts: &str, thread_ts: Option<&str>, author: &str, text: &str) -> Message {
        Message {
            conv: conv.into(),
            ts: ts.into(),
            thread_ts: thread_ts.map(str::to_owned),
            author: author.into(),
            text: text.into(),
            edited: false,
        }
    }

    fn empty_app() -> App {
        App {
            tab: Tab::Feed,
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
            self_id: "SELF".to_string(),
            keywords: Vec::new(),
            arrival_seq: 0,
            divider_mark: 0,
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

        let rows = app.feed_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, RowKind::Message);
        assert_eq!(rows[1].kind, RowKind::ThreadMarker { replies: 2, expanded: false });
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
        let rows = app.feed_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].kind, RowKind::ThreadMarker { replies: 1, expanded: false });
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
    fn mention_rows_are_newest_first() {
        let mut app = empty_app();
        app.conv_kinds.insert("D1".to_string(), ConvKind::Im);
        app.apply(SocketEvent::Message(msg("D1", "1.0", None, "U1", "older dm")));
        app.apply(SocketEvent::Message(msg("D1", "2.0", None, "U1", "newer dm")));

        let rows = app.mention_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "newer dm");
        assert_eq!(rows[1].text, "older dm");
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
    fn a_message_seen_via_socket_and_poll_appears_once() {
        let mut app = empty_app();
        // Simulates the same (conv, ts) arriving twice: once via the socket, once via a poll
        // backfill re-fetch (poll_tick re-runs history() and re-applies through the same
        // upsert path apply() uses).
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "hello")));
        app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "hello")));
        app.touch();
        assert_eq!(app.feed_rows().len(), 1);
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

    // ---- resolve_channels ---------------------------------------------------------------------

    #[test]
    fn resolve_channels_matches_by_name_stripping_the_hash() {
        let all = vec![
            conv("C1", "eng-infra", ConvKind::Channel),
            conv("C2", "releases", ConvKind::Channel),
        ];
        let resolved = resolve_channels(&["#eng-infra".to_string()], false, &all).unwrap();
        assert_eq!(resolved, vec![conv("C1", "eng-infra", ConvKind::Channel)]);
    }

    #[test]
    fn resolve_channels_errors_naming_an_unknown_channel() {
        let all = vec![conv("C1", "eng-infra", ConvKind::Channel)];
        let error = resolve_channels(&["#nope".to_string()], false, &all).unwrap_err();
        assert!(error.contains("#nope"), "{error}");
    }

    #[test]
    fn resolve_channels_includes_all_dms_when_enabled() {
        let all = vec![
            conv("C1", "eng-infra", ConvKind::Channel),
            conv("D1", "U9", ConvKind::Im),
            conv("M1", "mpdm-a-b", ConvKind::Mpim),
        ];
        let resolved = resolve_channels(&["#eng-infra".to_string()], true, &all).unwrap();
        assert_eq!(resolved.len(), 3);
    }

    #[test]
    fn resolve_channels_excludes_dms_when_disabled() {
        let all = vec![conv("C1", "eng-infra", ConvKind::Channel), conv("D1", "U9", ConvKind::Im)];
        let resolved = resolve_channels(&["#eng-infra".to_string()], false, &all).unwrap();
        assert_eq!(resolved.len(), 1);
    }

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
}
