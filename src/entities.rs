//! Slack text-entity resolution and Mentions-tab attention detection.
//!
//! See `docs/superpowers/specs/2026-07-12-herdr-slackr-design.md`. Both functions are pure:
//! `resolve` takes id→name lookups as closures instead of touching a cache directly, and
//! `is_mention` only ever inspects the raw `Message` it is handed. Neither performs I/O.

use crate::model::{ConvKind, Message};

/// Resolve Slack's inline entity syntax and HTML escapes for display:
/// - `<@U123>` → `@name` via `user_name`; an unresolvable id renders literally as `@U123`.
/// - `<#C123|eng>` → `#eng` (the label Slack already sent, used as-is).
/// - `<#C123>` (no label) → `#name` via `conv_name`; an unresolvable id renders `#C123`.
/// - `<https://x|label>` → `label`; `<https://x>` (no label) → `https://x` itself.
/// - `&lt;`, `&gt;`, `&amp;` unescape to `<`, `>`, `&` (order matters: `&amp;` last, so a
///   message that literally contained `&lt;` — sent by Slack as `&amp;lt;` — round-trips
///   to `&lt;` instead of being double-unescaped into `<`).
///
/// An unmatched `<` (no closing `>`) is passed through literally rather than eating the
/// rest of the message.
pub fn resolve(
    text: &str,
    user_name: impl Fn(&str) -> Option<String>,
    conv_name: impl Fn(&str) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&unescape(&rest[..start]));
        rest = &rest[start + 1..];
        let Some(end) = rest.find('>') else {
            out.push('<');
            out.push_str(&unescape(rest));
            return out;
        };
        let entity = &rest[..end];
        out.push_str(&unescape(&resolve_entity(entity, &user_name, &conv_name)));
        rest = &rest[end + 1..];
    }
    out.push_str(&unescape(rest));
    out
}

fn resolve_entity(
    entity: &str,
    user_name: &impl Fn(&str) -> Option<String>,
    conv_name: &impl Fn(&str) -> Option<String>,
) -> String {
    if let Some(id) = entity.strip_prefix('@') {
        let id = id.split('|').next().unwrap_or(id);
        return match user_name(id) {
            Some(name) => format!("@{name}"),
            None => format!("@{id}"),
        };
    }
    if let Some(rest) = entity.strip_prefix('#') {
        let mut parts = rest.splitn(2, '|');
        let id = parts.next().unwrap_or("");
        if let Some(label) = parts.next() {
            return format!("#{label}");
        }
        return match conv_name(id) {
            Some(name) => format!("#{name}"),
            None => format!("#{id}"),
        };
    }
    let mut parts = entity.splitn(2, '|');
    let url = parts.next().unwrap_or("");
    if let Some(label) = parts.next() { label.to_owned() } else { url.to_owned() }
}

/// Unescape Slack's three HTML entities. `&amp;` is replaced last so a message that
/// literally contained `&lt;` (sent over the wire as `&amp;lt;`) round-trips correctly
/// instead of being double-unescaped into `<`.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Whether `msg` belongs on the Mentions tab: a literal `<@{self_id}>` token in the raw
/// text, any Im/Mpim message (a DM is inherently addressed to you), or a case-insensitive
/// keyword hit. Keyword matching is a plain substring test on the lowercased raw text —
/// deliberately *not* word-bounded, so a keyword like `"cat"` also fires inside
/// `"concatenate"`. That's a conscious choice (see brief): Slack keyword alerts are meant
/// to be recall-biased, and word-boundary detection needs a tokenizer this crate doesn't
/// otherwise carry. An empty keyword is skipped rather than matching every message.
#[must_use]
pub fn is_mention(msg: &Message, kind: ConvKind, self_id: &str, keywords: &[String]) -> bool {
    if matches!(kind, ConvKind::Im | ConvKind::Mpim) {
        return true;
    }
    let mention_token = format!("<@{self_id}>");
    if msg.text.contains(&mention_token) {
        return true;
    }
    keyword_hit(&msg.text, keywords)
}

/// Case-insensitive substring keyword test shared by `is_mention`'s keyword branch and Focus
/// mode's `focus_keywords` check (`crate::app::qualifies_for_focus`) — same matching rule,
/// deliberately not word-bounded (see `is_mention`'s doc for why), applied to two distinct
/// config keys (`keywords` for Mentions, `focus_keywords` for Focus) that must never be
/// conflated even though the substring rule they use is identical. An empty keyword is skipped
/// rather than matching every message.
#[must_use]
pub fn keyword_hit(text: &str, keywords: &[String]) -> bool {
    let lower = text.to_lowercase();
    keywords.iter().any(|kw| !kw.is_empty() && lower.contains(&kw.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::{is_mention, resolve};
    use crate::model::{ConvKind, Message};

    fn users(id: &str) -> Option<String> {
        if id == "U1" { Some("dan".to_owned()) } else { None }
    }

    fn convs(id: &str) -> Option<String> {
        if id == "C1" { Some("eng".to_owned()) } else { None }
    }

    fn msg(text: &str) -> Message {
        Message {
            conv: "C1".into(),
            ts: "1.000001".into(),
            thread_ts: None,
            author: "U1".into(),
            text: text.into(),
            edited: false,
            reply_count: None,
            reactions: Vec::new(),
        }
    }

    #[test]
    fn resolves_known_user_mention() {
        assert_eq!(resolve("<@U1> hi", users, convs), "@dan hi");
    }

    #[test]
    fn unknown_user_renders_the_raw_id() {
        assert_eq!(resolve("<@U999> hi", users, convs), "@U999 hi");
    }

    #[test]
    fn channel_with_label_uses_the_label_verbatim() {
        assert_eq!(resolve("see <#C1|eng>", users, convs), "see #eng");
    }

    #[test]
    fn channel_without_label_resolves_via_lookup() {
        assert_eq!(resolve("see <#C1>", users, convs), "see #eng");
    }

    #[test]
    fn channel_without_label_falls_back_to_id_when_unknown() {
        assert_eq!(resolve("see <#C999>", users, convs), "see #C999");
    }

    #[test]
    fn link_with_label_uses_the_label() {
        assert_eq!(resolve("go <https://x|here>", users, convs), "go here");
    }

    #[test]
    fn link_without_label_uses_the_url() {
        assert_eq!(resolve("go <https://x>", users, convs), "go https://x");
    }

    #[test]
    fn html_entities_are_unescaped() {
        assert_eq!(resolve("a &lt;b&gt; &amp; c", users, convs), "a <b> & c");
    }

    #[test]
    fn double_escaped_lt_round_trips_without_double_unescaping() {
        // A message that literally contained "&lt;" arrives over the wire as "&amp;lt;".
        assert_eq!(resolve("&amp;lt;", users, convs), "&lt;");
    }

    #[test]
    fn mixed_sentence_resolves_every_form_together() {
        let text = "<@U1>: check <#C1|eng> and <https://x|docs> re: R&amp;D <@U999>";
        assert_eq!(resolve(text, users, convs), "@dan: check #eng and docs re: R&D @U999");
    }

    #[test]
    fn unmatched_angle_bracket_is_passed_through_literally() {
        assert_eq!(resolve("a < b", users, convs), "a < b");
    }

    #[test]
    fn is_mention_hits_on_literal_self_mention_token() {
        let m = msg("hey <@SELF> got a sec?");
        assert!(is_mention(&m, ConvKind::Channel, "SELF", &[]));
    }

    #[test]
    fn is_mention_always_true_for_im_and_mpim() {
        let m = msg("no mention or keyword here");
        assert!(is_mention(&m, ConvKind::Im, "SELF", &[]));
        assert!(is_mention(&m, ConvKind::Mpim, "SELF", &[]));
    }

    #[test]
    fn is_mention_keyword_match_is_case_insensitive() {
        let m = msg("there is an URGENT update");
        let keywords = vec!["urgent".to_owned()];
        assert!(is_mention(&m, ConvKind::Channel, "SELF", &keywords));
    }

    #[test]
    fn is_mention_keyword_matches_as_a_substring_inside_a_word() {
        // Documented policy: substring match, not word-bounded — "cat" fires inside
        // "concatenate" too.
        let m = msg("please concatenate the files");
        let keywords = vec!["cat".to_owned()];
        assert!(is_mention(&m, ConvKind::Channel, "SELF", &keywords));
    }

    #[test]
    fn is_mention_false_when_channel_message_has_no_mention_or_keyword() {
        let m = msg("just a regular update, nothing to see");
        let keywords = vec!["urgent".to_owned()];
        assert!(!is_mention(&m, ConvKind::Channel, "SELF", &keywords));
    }
}
