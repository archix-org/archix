use std::cmp::Ordering;

/// Compares two package versions of the form `[epoch:]pkgver[-pkgrel]` the way pacman's `vercmp` does.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    let a = Version::parse(a);
    let b = Version::parse(b);
    compare_digits(a.epoch.as_bytes(), b.epoch.as_bytes())
        .then_with(|| compare_part(a.pkgver, b.pkgver))
        .then_with(|| match (a.pkgrel, b.pkgrel) {
            (Some(x), Some(y)) => compare_part(x, y),
            _ => Ordering::Equal,
        })
}

struct Version<'a> {
    epoch: &'a str,
    pkgver: &'a str,
    pkgrel: Option<&'a str>,
}

impl<'a> Version<'a> {
    fn parse(v: &'a str) -> Self {
        // Only a run of digits (possibly empty) directly before the first ':' is an epoch.
        let digits = v.bytes().take_while(u8::is_ascii_digit).count();
        let (epoch, rest) = match v.as_bytes().get(digits) {
            Some(b':') => (&v[..digits], &v[digits + 1..]),
            _ => ("", v),
        };
        let (pkgver, pkgrel) = match rest.rsplit_once('-') {
            Some((ver, rel)) => (ver, Some(rel)),
            None => (rest, None),
        };
        Version { epoch, pkgver, pkgrel }
    }
}

/// Compares one part (pkgver or pkgrel) segment by segment.
fn compare_part(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    while !a.is_empty() && !b.is_empty() {
        let (seps_a, rest_a) = split_run(a, is_separator);
        let (seps_b, rest_b) = split_run(b, is_separator);
        (a, b) = (rest_a, rest_b);
        if a.is_empty() || b.is_empty() {
            break;
        }
        // A longer separator run between segments ranks newer, so `1..0` > `1.0`.
        if seps_a.len() != seps_b.len() {
            return seps_a.len().cmp(&seps_b.len());
        }
        let numeric_a = a[0].is_ascii_digit();
        let numeric_b = b[0].is_ascii_digit();
        if numeric_a != numeric_b {
            return if numeric_a { Ordering::Greater } else { Ordering::Less };
        }
        let kind = if numeric_a {
            u8::is_ascii_digit
        } else {
            u8::is_ascii_alphabetic
        };
        let (seg_a, rest_a) = split_run(a, kind);
        let (seg_b, rest_b) = split_run(b, kind);
        let ord = if numeric_a {
            compare_digits(seg_a, seg_b)
        } else {
            seg_a.cmp(seg_b)
        };
        if ord != Ordering::Equal {
            return ord;
        }
        (a, b) = (rest_a, rest_b);
    }
    // At least one side is used up; whatever the other has left decides.
    leftover(a).then(leftover(b).reverse())
}

/// Ranks a leftover tail against nothing: a tail starting with a letter is older, any other is newer.
fn leftover(tail: &[u8]) -> Ordering {
    match tail.first() {
        None => Ordering::Equal,
        Some(c) if c.is_ascii_alphabetic() => Ordering::Less,
        Some(_) => Ordering::Greater,
    }
}

/// Compares two runs of ASCII digits by numeric value, with no width limit.
fn compare_digits(a: &[u8], b: &[u8]) -> Ordering {
    let a = trim_zeros(a);
    let b = trim_zeros(b);
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
    let zeros = digits.iter().take_while(|&&c| c == b'0').count();
    &digits[zeros..]
}

fn is_separator(c: &u8) -> bool {
    !c.is_ascii_alphanumeric()
}

/// Splits off the longest prefix whose bytes all satisfy `pred`.
fn split_run(s: &[u8], pred: impl Fn(&u8) -> bool) -> (&[u8], &[u8]) {
    let n = s.iter().take_while(|c| pred(c)).count();
    s.split_at(n)
}

#[cfg(test)]
mod tests {
    use super::vercmp;
    use std::cmp::Ordering::{self, Equal, Greater, Less};

    const CASES: &[(&str, &str, Ordering)] = &[
        ("1.0", "1.0", Equal),
        ("1.0", "1.1", Less),
        ("1.2.10", "1.2.9", Greater),
        ("1.010", "1.10", Equal),
        ("001.002", "1.2", Equal),
        ("1.0", "1.0.0", Less),
        ("1.0", "1", Greater),
        ("1.0a", "1.0", Less),
        ("1.0rc1", "1.0", Less),
        ("1.0alpha", "1.0beta", Less),
        ("1.0rc10", "1.0rc9", Greater),
        ("1.0a", "1.0.1", Less),
        ("1.0a", "1.0.a", Less),
        ("1a", "a1", Greater),
        ("A", "a", Less),
        ("1.0_alpha", "1.0", Greater),
        ("1.0~rc1", "1.0", Greater),
        ("1.0+git", "1.0.1", Less),
        ("1.0", "1_0", Equal),
        ("1.0+", "1.0_", Equal),
        ("1.0", "1.0.", Less),
        ("1.0.", "1.0..", Equal),
        ("1..0", "1.0", Greater),
        ("1.0", ".1.0", Less),
        ("", "1", Less),
        ("", "a", Greater),
        ("1", "1.a", Less),
        ("1.2.r45.gabc1234", "1.2.r46.g0", Less),
        ("1.2.r45.gabc1234", "1.2.1", Less),
        ("1:1.0", "2:0.1", Less),
        ("1:1.0", "1.0", Greater),
        ("0:1.0", "1.0", Equal),
        (":1.0", "1.0", Equal),
        ("a:2", "a.2", Equal),
        ("1:2:3", "1:2", Greater),
        ("1.0-1", "1.0-2", Less),
        ("1.0-10", "1.0-9", Greater),
        ("1.0-1", "1.0", Equal),
        ("1.0-", "1.0-1", Less),
        ("1.0-1-1", "1.0-2", Greater),
        ("1.0.1-1", "1.0-1-1", Equal),
        ("18446744073709551616", "18446744073709551615", Greater),
        ("123456789012345678901234567890", "123456789012345678901234567891", Less),
    ];

    #[test]
    fn representative_pairs() {
        for &(a, b, want) in CASES {
            assert_eq!(vercmp(a, b), want, "vercmp({a:?}, {b:?})");
            assert_eq!(vercmp(b, a), want.reverse(), "vercmp({b:?}, {a:?})");
        }
    }

    #[test]
    fn matches_pacman_oracle() {
        let data = include_str!("vercmp-oracle.tsv");
        let mut checked = 0;
        let mut mismatches = Vec::new();
        for line in data.lines().filter(|l| !l.starts_with('#')) {
            let mut fields = line.splitn(3, '\t');
            let (a, b, result) = match (fields.next(), fields.next(), fields.next()) {
                (Some(a), Some(b), Some(r)) => (a, b, r),
                _ => panic!("malformed oracle line {line:?}"),
            };
            let want = match result {
                "-1" => Less,
                "0" => Equal,
                "1" => Greater,
                _ => panic!("bad result in oracle line {line:?}"),
            };
            if vercmp(a, b) != want {
                mismatches.push(format!("{a:?} vs {b:?}: want {want:?}, got {:?}", vercmp(a, b)));
            }
            checked += 1;
        }
        assert!(checked >= 3000, "oracle corpus too small: {checked}");
        assert!(
            mismatches.is_empty(),
            "{} mismatches:\n{}",
            mismatches.len(),
            mismatches.join("\n")
        );
    }
}
