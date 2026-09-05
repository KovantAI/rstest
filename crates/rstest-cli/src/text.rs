//! Small string helpers shared across modules.

/// Truncate `s` in place to at most `max` bytes, cutting on a UTF-8 char
/// boundary. Plain `String::truncate(max)` panics when byte `max` splits a
/// multibyte char — routine in tracebacks / parametrize-id samples, which
/// carry arbitrary Unicode. This cuts at the largest boundary `<= max`
/// (never mid-char), so it can't panic; the result is at most `max` bytes.
pub fn truncate_on_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

#[cfg(test)]
mod tests {
    use super::truncate_on_boundary;

    #[test]
    fn shorter_than_max_untouched() {
        let mut s = "hello".to_string();
        truncate_on_boundary(&mut s, 20_000);
        assert_eq!(s, "hello");
    }

    #[test]
    fn ascii_cuts_exactly() {
        let mut s = "abcdef".to_string();
        truncate_on_boundary(&mut s, 3);
        assert_eq!(s, "abc");
    }

    #[test]
    fn multibyte_at_boundary_does_not_panic() {
        // "é" is 2 bytes (0xC3 0xA9). Cutting at byte 1 would split it.
        let mut s = "aé".to_string(); // bytes: a(1) + é(2) = 3
        truncate_on_boundary(&mut s, 2); // byte 2 is mid-'é'
        assert_eq!(s, "a"); // backs up to the boundary at byte 1
        assert!(s.len() <= 2);
    }

    #[test]
    fn cut_on_exact_boundary_keeps_char() {
        let mut s = "aé".to_string();
        truncate_on_boundary(&mut s, 3); // exactly the full string
        assert_eq!(s, "aé");
    }

    #[test]
    fn multibyte_first_char_over_limit_yields_empty() {
        let mut s = "€xyz".to_string(); // '€' is 3 bytes
        truncate_on_boundary(&mut s, 2); // no boundary in (0,2] except 0
        assert_eq!(s, "");
    }
}
