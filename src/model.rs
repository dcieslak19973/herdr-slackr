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

/// A subscribed conversation: id (stable), display name, and kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub id: String,
    pub name: String,
    pub kind: ConvKind,
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

#[cfg(test)]
mod tests {
    use super::{ConvKind, ts_cmp};
    use std::cmp::Ordering;

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
}
