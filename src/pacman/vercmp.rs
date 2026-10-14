use std::cmp::Ordering;

/// Compares two pacman versions (`[epoch:]version[-release]`) exactly like libalpm's `alpm_pkg_vercmp`.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (epoch_a, version_a, release_a) = split_evr(a);
    let (epoch_b, version_b, release_b) = split_evr(b);
    rpmvercmp(epoch_a, epoch_b)
        .then_with(|| rpmvercmp(version_a, version_b))
        .then_with(|| match (release_a, release_b) {
            (Some(x), Some(y)) => rpmvercmp(x, y),
            _ => Ordering::Equal,
        })
}

fn split_evr(evr: &str) -> (&str, &str, Option<&str>) {
    let digits = evr.bytes().take_while(u8::is_ascii_digit).count();
    let (epoch, rest) = match evr[digits..].strip_prefix(':') {
        Some(rest) if digits > 0 => (&evr[..digits], rest),
        Some(rest) => ("0", rest),
        None => ("0", evr),
    };
    match rest.rfind('-') {
        Some(i) => (epoch, &rest[..i], Some(&rest[i + 1..])),
        None => (epoch, rest, None),
    }
}

fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (sep_start_a, sep_start_b) = (i, j);
        while i < a.len() && !a[i].is_ascii_alphanumeric() {
            i += 1;
        }
        while j < b.len() && !b[j].is_ascii_alphanumeric() {
            j += 1;
        }
        if i == a.len() || j == b.len() {
            break;
        }
        let (sep_a, sep_b) = (i - sep_start_a, j - sep_start_b);
        if sep_a != sep_b {
            return sep_a.cmp(&sep_b);
        }
        let numeric = a[i].is_ascii_digit();
        let class = |c: &u8| {
            if numeric {
                c.is_ascii_digit()
            } else {
                c.is_ascii_alphabetic()
            }
        };
        let end_a = i + a[i..].iter().take_while(|c| class(c)).count();
        let end_b = j + b[j..].iter().take_while(|c| class(c)).count();
        if j == end_b {
            return if numeric { Ordering::Greater } else { Ordering::Less };
        }
        let (mut seg_a, mut seg_b) = (&a[i..end_a], &b[j..end_b]);
        if numeric {
            seg_a = trim_zeros(seg_a);
            seg_b = trim_zeros(seg_b);
            let by_len = seg_a.len().cmp(&seg_b.len());
            if by_len != Ordering::Equal {
                return by_len;
            }
        }
        let by_bytes = seg_a.cmp(seg_b);
        if by_bytes != Ordering::Equal {
            return by_bytes;
        }
        i = end_a;
        j = end_b;
    }
    let rest_a = &a[i..];
    let rest_b = &b[j..];
    if rest_a.is_empty() && rest_b.is_empty() {
        return Ordering::Equal;
    }
    let a_alpha = rest_a.first().is_some_and(u8::is_ascii_alphabetic);
    let b_alpha = rest_b.first().is_some_and(u8::is_ascii_alphabetic);
    if (rest_a.is_empty() && !b_alpha) || a_alpha {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
    let zeros = digits.iter().take_while(|&&c| c == b'0').count();
    &digits[zeros..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ordering::{Equal, Greater, Less};

    #[test]
    fn matches_libalpm() {
        let cases = [
            ("1.5.0", "1.5.0", Equal),
            ("1.5.1", "1.5.0", Greater),
            ("1.5.1", "1.5", Greater),
            ("1.5.0", "1.5", Greater),
            ("1.5b", "1.5", Less),
            ("1.0rc", "1.0", Less),
            ("1.0a", "1.0alpha", Less),
            ("1.10", "1.9", Greater),
            ("1.01", "1.1", Equal),
            ("1.0-1", "1.0-2", Less),
            ("1.0", "1.0-2", Equal),
            ("1:1.0", "2.0", Greater),
            ("0:1.0", "1.0", Equal),
            ("2.44+r50+g1848099f063e-1", "2.44+r49+gabc-1", Greater),
            ("1:1.6.9-1", "1.6.10-1", Greater),
            ("6.17.1.arch1-1", "6.17.1.arch2-1", Less),
        ];
        for (a, b, expected) in cases {
            assert_eq!(vercmp(a, b), expected, "{a} vs {b}");
            assert_eq!(vercmp(b, a), expected.reverse(), "{b} vs {a}");
        }
    }
}
