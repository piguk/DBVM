//! The change list written to agent_steps and printed by agent-log must be valid JSON for any path,
//! including ones with quotes, backslashes, newlines, tabs, and other control characters.
use selfdb::agent::json_string;

#[test]
fn json_string_round_trips_every_control_character() {
    // every byte from 0x00 to 0x7f, plus the tricky printable ones
    let mut s: String = (0u8..0x80).map(|b| b as char).collect();
    s.push_str("\"quoted\" back\\slash é 中文 🙂");
    let lit = json_string(&s);
    let back: String = serde_json::from_str(&lit).expect("must be valid JSON");
    assert_eq!(back, s, "round trip must be exact");
}

#[test]
fn json_string_escapes_newline_and_tab() {
    assert_eq!(json_string("a\nb\tc"), "\"a\\nb\\tc\"");
    assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
}
