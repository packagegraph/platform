//! RPM version/release comparison (rpmvercmp) and epoch-aware EVR comparison.
use std::cmp::Ordering;

fn strip_leading_zeros(s: &[u8]) -> &[u8] {
    let mut i = 0;
    while i + 1 < s.len() && s[i] == b'0' { i += 1; }
    &s[i..]
}

/// Compare two RPM version (or release) strings per RPM's rpmvercmp rules.
pub fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b { return Ordering::Equal; }
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    let is_sep = |c: u8| !c.is_ascii_alphanumeric() && c != b'~' && c != b'^';
    loop {
        while !a.is_empty() && is_sep(a[0]) { a = &a[1..]; }
        while !b.is_empty() && is_sep(b[0]) { b = &b[1..]; }

        // tilde: older than anything, including empty
        if a.first() == Some(&b'~') || b.first() == Some(&b'~') {
            if a.first() != Some(&b'~') { return Ordering::Greater; }
            if b.first() != Some(&b'~') { return Ordering::Less; }
            a = &a[1..]; b = &b[1..]; continue;
        }
        // caret: newer than the string ending, older than a following segment
        if a.first() == Some(&b'^') || b.first() == Some(&b'^') {
            if a.is_empty() { return Ordering::Less; }
            if b.is_empty() { return Ordering::Greater; }
            if a.first() != Some(&b'^') { return Ordering::Greater; }
            if b.first() != Some(&b'^') { return Ordering::Less; }
            a = &a[1..]; b = &b[1..]; continue;
        }

        if a.is_empty() || b.is_empty() { break; }

        let isnum = a[0].is_ascii_digit();
        let take = |s: &[u8], num: bool| -> usize {
            s.iter().position(|&c| if num { !c.is_ascii_digit() } else { !c.is_ascii_alphabetic() })
                .unwrap_or(s.len())
        };
        let na = take(a, isnum);
        let (seg_a, rest_a) = a.split_at(na);
        let nb = take(b, isnum);
        let (seg_b, rest_b) = b.split_at(nb);

        // a is num, b starts alpha (nb==0 under num rule) => numeric newer
        if isnum && seg_b.is_empty() { return Ordering::Greater; }
        // a is alpha, b starts num => numeric(b) newer => a older
        if !isnum && seg_b.is_empty() { return Ordering::Less; }

        let ord = if isnum {
            let (sa, sb) = (strip_leading_zeros(seg_a), strip_leading_zeros(seg_b));
            if sa.len() != sb.len() { sa.len().cmp(&sb.len()) } else { sa.cmp(sb) }
        } else {
            seg_a.cmp(seg_b)
        };
        if ord != Ordering::Equal { return ord; }
        a = rest_a; b = rest_b;
    }
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => unreachable!(),
    }
}

/// Epoch-aware EVR comparison.
pub fn evr_cmp(ea: i64, va: &str, ra: &str, eb: i64, vb: &str, rb: &str) -> Ordering {
    match ea.cmp(&eb) {
        Ordering::Equal => match rpmvercmp(va, vb) {
            Ordering::Equal => rpmvercmp(ra, rb),
            o => o,
        },
        o => o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn rpmvercmp_canonical_vectors() {
        assert_eq!(rpmvercmp("1.0", "1.0"), Equal);
        assert_eq!(rpmvercmp("1.0", "2.0"), Less);
        assert_eq!(rpmvercmp("2.0", "1.0"), Greater);
        assert_eq!(rpmvercmp("1.0.1", "1.0"), Greater);
        assert_eq!(rpmvercmp("1.0", "1.0.1"), Less);
        // leading zeros / numeric-length rules
        assert_eq!(rpmvercmp("1.0010", "1.9"), Greater);
        assert_eq!(rpmvercmp("1.05", "1.5"), Equal);
        // numeric segment newer than alpha
        assert_eq!(rpmvercmp("1.0", "1.0a"), Less);
        assert_eq!(rpmvercmp("5.5p1", "5.5p2"), Less);
        // tilde sorts before everything
        assert_eq!(rpmvercmp("1.0~rc1", "1.0"), Less);
        assert_eq!(rpmvercmp("1.0~rc1", "1.0~rc2"), Less);
        // caret sorts after
        assert_eq!(rpmvercmp("1.0^", "1.0"), Greater);
        // real-world el9 releases
        assert_eq!(rpmvercmp("9.el9", "6.el9_1"), Greater);
    }

    #[test]
    fn evr_cmp_epoch_dominates() {
        assert_eq!(evr_cmp(1, "1.0", "1", 0, "2.0", "1"), Greater);
        assert_eq!(evr_cmp(0, "1.0", "1", 0, "1.0", "2"), Less);
        assert_eq!(evr_cmp(0, "3.5.5", "6.el9_8", 0, "3.5.5", "6.el9_8"), Equal);
    }
}
