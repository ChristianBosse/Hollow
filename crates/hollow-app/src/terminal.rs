/// Strips control characters (including ESC) from text that came from a
/// child or a website, so a compromised renderer can't drive the terminal
/// with escape sequences.
pub fn sanitize(s: &str) -> String {
    s.chars().filter(|c| *c == '\n' || *c == '\t' || !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_escape_sequences() {
        assert_eq!(super::sanitize("ok\u{1b}[2Jgone\u{7}\nnext"), "ok[2Jgone\nnext");
    }
}
