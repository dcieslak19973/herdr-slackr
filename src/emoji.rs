//! Slack emoji-shortcode rendering for reactions: a small vendored table of the shortcodes
//! that actually occur as workplace reactions, mapped to their Unicode emoji, with a `:name:`
//! fallback for everything else.
//!
//! Deliberately a table and not an emoji crate: the full Unicode emoji set is thousands of
//! entries pulled in through a dependency this project's closed dep list doesn't carry, while
//! real-world reaction usage is overwhelmingly concentrated in a few dozen names — and a
//! custom workspace emoji (the long tail here) has no Unicode form at all, so `:name:` is the
//! best any terminal can do for it regardless. Skin-tone suffixes (`+1::skin-tone-3`) are
//! stripped before lookup: the base emoji is the readable part in a monospace pane.

/// Slack reaction shortcodes → Unicode, ordered roughly by real-world reaction frequency.
/// Aliases (e.g. `+1`/`thumbsup`) are separate rows pointing at the same emoji.
const TABLE: &[(&str, &str)] = &[
    ("+1", "👍"),
    ("thumbsup", "👍"),
    ("-1", "👎"),
    ("thumbsdown", "👎"),
    ("heart", "❤️"),
    ("hearts", "💕"),
    ("tada", "🎉"),
    ("joy", "😂"),
    ("laughing", "😆"),
    ("smile", "😄"),
    ("grinning", "😀"),
    ("slightly_smiling_face", "🙂"),
    ("wink", "😉"),
    ("eyes", "👀"),
    ("rocket", "🚀"),
    ("fire", "🔥"),
    ("100", "💯"),
    ("clap", "👏"),
    ("pray", "🙏"),
    ("raised_hands", "🙌"),
    ("ok_hand", "👌"),
    ("wave", "👋"),
    ("muscle", "💪"),
    ("point_up", "☝️"),
    ("point_right", "👉"),
    ("white_check_mark", "✅"),
    ("heavy_check_mark", "✔️"),
    ("x", "❌"),
    ("heavy_plus_sign", "➕"),
    ("thinking_face", "🤔"),
    ("sob", "😭"),
    ("cry", "😢"),
    ("sweat_smile", "😅"),
    ("melting_face", "🫠"),
    ("skull", "💀"),
    ("shrug", "🤷"),
    ("man-shrugging", "🤷‍♂️"),
    ("woman-shrugging", "🤷‍♀️"),
    ("facepalm", "🤦"),
    ("bulb", "💡"),
    ("warning", "⚠️"),
    ("question", "❓"),
    ("exclamation", "❗"),
    ("star", "⭐"),
    ("sparkles", "✨"),
    ("sunglasses", "😎"),
    ("saluting_face", "🫡"),
    ("handshake", "🤝"),
    ("crossed_fingers", "🤞"),
    ("partying_face", "🥳"),
    ("star-struck", "🤩"),
    ("exploding_head", "🤯"),
    ("face_with_rolling_eyes", "🙄"),
    ("neutral_face", "😐"),
    ("grimacing", "😬"),
    ("hugging_face", "🤗"),
    ("wave_dash", "〰️"),
    ("checkered_flag", "🏁"),
    ("hourglass", "⌛"),
    ("stopwatch", "⏱️"),
    ("lock", "🔒"),
    ("unlock", "🔓"),
    ("mag", "🔍"),
    ("bug", "🐛"),
    ("bell", "🔔"),
    ("zap", "⚡"),
    ("boom", "💥"),
    ("ship", "🚢"),
    ("shipit", "🚢"),
    ("+1_tone", "👍"),
    ("coffee", "☕"),
    ("beers", "🍻"),
    ("pizza", "🍕"),
    ("cake", "🍰"),
    ("robot_face", "🤖"),
    ("ghost", "👻"),
    ("see_no_evil", "🙈"),
    ("clown_face", "🤡"),
    ("salute", "🫡"),
];

/// Render one Slack reaction shortcode: the Unicode emoji when the (skin-tone-stripped) name
/// is in [`TABLE`], else the `:name:` shortcode form — the honest rendering for a custom
/// workspace emoji, which has no Unicode form a terminal could show.
#[must_use]
pub fn render_name(name: &str) -> String {
    let base = name.split("::").next().unwrap_or(name);
    match TABLE.iter().find(|(n, _)| *n == base) {
        Some((_, emoji)) => (*emoji).to_string(),
        None => format!(":{base}:"),
    }
}

#[cfg(test)]
mod tests {
    use super::render_name;

    #[test]
    fn common_reactions_render_as_unicode() {
        assert_eq!(render_name("+1"), "👍");
        assert_eq!(render_name("thumbsup"), "👍");
        assert_eq!(render_name("tada"), "🎉");
        assert_eq!(render_name("white_check_mark"), "✅");
    }

    #[test]
    fn a_skin_tone_suffix_is_stripped_before_lookup() {
        assert_eq!(render_name("+1::skin-tone-3"), "👍");
        assert_eq!(render_name("wave::skin-tone-5"), "👋");
    }

    #[test]
    fn an_unknown_or_custom_emoji_falls_back_to_the_shortcode_form() {
        assert_eq!(render_name("party-parrot"), ":party-parrot:");
        assert_eq!(render_name("party-parrot::skin-tone-2"), ":party-parrot:");
    }
}
