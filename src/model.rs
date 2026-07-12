//! Slack message model: conversations and messages, plus the ts comparator.
//!
//! See `docs/superpowers/specs/2026-07-12-herdr-slackr-design.md`. `Message` carries raw
//! Slack text; entity resolution (`crate::entities::resolve`) happens at render time, not
//! here, so this module stays pure and I/O-free.

/// The four Slack conversation kinds this pane subscribes to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConvKind {
    Channel,
    Group,
    Im,
    Mpim,
}

/// A subscribed conversation: id (stable), display name, kind, and last-activity stamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub id: String,
    pub name: String,
    pub kind: ConvKind,
    /// `conversations.list`'s `updated` field (millisecond epoch of the conversation's last
    /// activity), when Slack's payload includes it. `None` when absent — some workspace
    /// payloads omit it, in which case DM-cap ranking (see [`resolve_channels`]) falls back to
    /// list order rather than treating a missing stamp as "oldest".
    pub updated: Option<u64>,
}

/// One Slack message. `ts` is both its identity within `conv` and its sort key; `thread_ts`
/// is `Some(root ts)` for a thread reply and `None` for a top-level message (including the
/// thread root itself, whose own `ts` *is* the thread's root ts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub conv: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub author: String,
    pub text: String,
    pub edited: bool,
}

/// Compare two Slack `ts` strings (e.g. `"1752300000.000100"`) numerically, not lexically.
/// Slack's ts is `<seconds>.<6-digit sequence>`; seconds are unpadded, so a plain string
/// compare gets 10-digit-vs-11-digit seconds wrong (`"999.1"` lexically beats `"1000.0"`
/// even though 1000 > 999). Splitting at `.` and comparing each half as an integer avoids
/// that trap. Malformed input (non-numeric halves) falls back to a lexical compare of the
/// original strings rather than panicking.
#[must_use]
pub fn ts_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    if let (Some(pa), Some(pb)) = (split_ts(a), split_ts(b)) { pa.cmp(&pb) } else { a.cmp(b) }
}

fn split_ts(ts: &str) -> Option<(u64, u64)> {
    let (secs, seq) = ts.split_once('.')?;
    let secs: u64 = secs.parse().ok()?;
    let seq: u64 = seq.parse().ok()?;
    Some((secs, seq))
}

/// Derive a sortable `(seconds, sequence)` key from a Slack `ts` string, for use as a
/// `BTreeMap` key (e.g. `crate::app`'s message store) where a total order — not a
/// lexical-fallback comparator like [`ts_cmp`] — is what the caller needs. Delegates to the
/// same parse [`ts_cmp`] uses; malformed input (non-numeric halves) yields `(0, 0)` rather
/// than panicking, so a handful of garbled timestamps just collide at the front of the map
/// instead of crashing the pane.
#[must_use]
pub fn ts_key(ts: &str) -> (u64, u32) {
    split_ts(ts).map_or((0, 0), |(secs, seq)| (secs, u32::try_from(seq).unwrap_or(u32::MAX)))
}

/// Resolve configured channel names (e.g. `"#eng-infra"`) to their [`Conversation`]s among
/// `all`, matching only `Channel`/`Group` kinds; an unresolved name is an error naming it (per
/// the design doc: "A configured channel name that resolves to nothing is an error naming the
/// channel"). When `dms` is true, every `Im`/`Mpim` conversation in `all` is appended — DMs are
/// subscribed as a class, not named individually in config — capped to the `dm_limit` most
/// recently active ones by [`Conversation::updated`] descending. `dm_limit == 0` excludes DMs
/// entirely even when `dms` is true. When the DM set exceeds `dm_limit` and *any* candidate DM
/// is missing `updated` (some workspace payloads omit it), ranking degrades to plain list order
/// — never scanning history to rank — and logs the degradation once via `logln!`. The single
/// shared home for this cap: it was previously duplicated verbatim in `app.rs` and `cli.rs`.
pub(crate) fn resolve_channels(
    config_channels: &[String],
    dms: bool,
    dm_limit: u32,
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
        let mut dm_convs: Vec<Conversation> = all
            .iter()
            .filter(|c| matches!(c.kind, ConvKind::Im | ConvKind::Mpim))
            .cloned()
            .collect();
        let limit = dm_limit as usize;
        if dm_convs.len() > limit {
            if dm_convs.iter().all(|c| c.updated.is_some()) {
                dm_convs.sort_by(|a, b| b.updated.cmp(&a.updated));
            } else {
                crate::logln!(
                    "resolve_channels: `updated` missing on one or more of {} DMs; falling back \
                     to list order for the dm_limit={dm_limit} cap",
                    dm_convs.len()
                );
            }
            dm_convs.truncate(limit);
        }
        out.extend(dm_convs);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{ConvKind, Conversation, resolve_channels, ts_cmp, ts_key};
    use std::cmp::Ordering;

    fn conv(id: &str, name: &str, kind: ConvKind) -> Conversation {
        Conversation { id: id.into(), name: name.into(), kind, updated: None }
    }

    fn conv_updated(id: &str, name: &str, kind: ConvKind, updated: u64) -> Conversation {
        Conversation { id: id.into(), name: name.into(), kind, updated: Some(updated) }
    }

    // ---- resolve_channels ---------------------------------------------------------------------

    #[test]
    fn resolve_channels_matches_by_name_stripping_the_hash() {
        let all = vec![
            conv("C1", "eng-infra", ConvKind::Channel),
            conv("C2", "releases", ConvKind::Channel),
        ];
        let resolved = resolve_channels(&["#eng-infra".to_string()], false, 20, &all).unwrap();
        assert_eq!(resolved, vec![conv("C1", "eng-infra", ConvKind::Channel)]);
    }

    #[test]
    fn resolve_channels_errors_naming_an_unknown_channel() {
        let all = vec![conv("C1", "eng-infra", ConvKind::Channel)];
        let error = resolve_channels(&["#nope".to_string()], false, 20, &all).unwrap_err();
        assert!(error.contains("#nope"), "{error}");
    }

    #[test]
    fn resolve_channels_includes_all_dms_when_enabled_and_under_the_cap() {
        let all = vec![
            conv("C1", "eng-infra", ConvKind::Channel),
            conv("D1", "U9", ConvKind::Im),
            conv("M1", "mpdm-a-b", ConvKind::Mpim),
        ];
        let resolved = resolve_channels(&["#eng-infra".to_string()], true, 20, &all).unwrap();
        assert_eq!(resolved.len(), 3);
    }

    #[test]
    fn resolve_channels_excludes_dms_when_disabled() {
        let all = vec![conv("C1", "eng-infra", ConvKind::Channel), conv("D1", "U9", ConvKind::Im)];
        let resolved = resolve_channels(&["#eng-infra".to_string()], false, 20, &all).unwrap();
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn resolve_channels_dm_limit_zero_excludes_dms_even_when_enabled() {
        let all = vec![conv("C1", "eng-infra", ConvKind::Channel), conv("D1", "U9", ConvKind::Im)];
        let resolved = resolve_channels(&["#eng-infra".to_string()], true, 0, &all).unwrap();
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn resolve_channels_caps_dms_to_the_most_recently_updated() {
        let all = vec![
            conv_updated("D1", "U1", ConvKind::Im, 100),
            conv_updated("D2", "U2", ConvKind::Im, 300),
            conv_updated("D3", "U3", ConvKind::Im, 200),
        ];
        let resolved = resolve_channels(&[], true, 2, &all).unwrap();
        assert_eq!(resolved.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["D2", "D3"]);
    }

    #[test]
    fn resolve_channels_falls_back_to_list_order_when_any_dm_is_missing_updated() {
        let all = vec![
            conv_updated("D1", "U1", ConvKind::Im, 100),
            conv("D2", "U2", ConvKind::Im), // no `updated`
            conv_updated("D3", "U3", ConvKind::Im, 999),
        ];
        let resolved = resolve_channels(&[], true, 2, &all).unwrap();
        // List order preserved (D1, D2), not ranked by `updated` (which would pick D3, D1).
        assert_eq!(resolved.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["D1", "D2"]);
    }

    #[test]
    fn conv_kind_is_a_plain_copyable_enum() {
        let kind = ConvKind::Channel;
        let copy = kind;
        assert_eq!(kind, copy);
        assert_ne!(ConvKind::Channel, ConvKind::Group);
    }

    #[test]
    fn ts_cmp_numeric_not_lexical_across_digit_widths() {
        // The lexical trap: "999.1" < "1000.0" as strings would be false (since '9' > '1'
        // as the first character), but numerically 999 < 1000 must hold.
        assert_eq!(ts_cmp("999.1", "1000.0"), Ordering::Less);
        assert_eq!(ts_cmp("1000.0", "999.1"), Ordering::Greater);
    }

    #[test]
    fn ts_cmp_equal_ts_is_equal() {
        assert_eq!(ts_cmp("1752300000.000100", "1752300000.000100"), Ordering::Equal);
    }

    #[test]
    fn ts_cmp_breaks_ties_on_sequence() {
        assert_eq!(ts_cmp("1.000001", "1.000002"), Ordering::Less);
        assert_eq!(ts_cmp("1.000002", "1.000001"), Ordering::Greater);
    }

    #[test]
    fn ts_cmp_falls_back_to_lexical_on_malformed_input() {
        // Not a panic site: a malformed ts still yields a total order via string compare.
        assert_eq!(ts_cmp("garbage", "garbage"), Ordering::Equal);
        assert_eq!(ts_cmp("abc", "abd"), Ordering::Less);
    }

    #[test]
    fn ts_key_splits_seconds_and_sequence() {
        assert_eq!(ts_key("1752300000.000100"), (1_752_300_000, 100));
    }

    #[test]
    fn ts_key_malformed_input_is_zero_not_a_panic() {
        assert_eq!(ts_key("garbage"), (0, 0));
    }
}
