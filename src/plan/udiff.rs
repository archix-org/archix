use std::fmt::Write as _;

const CONTEXT: usize = 3;
const MAX_CELLS: usize = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Keep,
    Del,
    Add,
}

fn ops(a: &[&str], b: &[&str]) -> Option<Vec<(Op, usize, usize)>> {
    let (n, m) = (a.len(), b.len());
    if (n + 1) * (m + 1) > MAX_CELLS {
        return None;
    }
    let mut lcs = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[at(i, j)] = if a[i] == b[j] {
                lcs[at(i + 1, j + 1)] + 1
            } else {
                lcs[at(i + 1, j)].max(lcs[at(i, j + 1)])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            out.push((Op::Keep, i, j));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[at(i + 1, j)] >= lcs[at(i, j + 1)]) {
            out.push((Op::Del, i, j));
            i += 1;
        } else {
            out.push((Op::Add, i, j));
            j += 1;
        }
    }
    Some(out)
}

/// A unified diff of two texts, or None when they're equal; a text that isn't UTF-8 or is too large gets a one-line note.
pub fn unified(old: &[u8], new: &[u8], path: &str) -> Option<String> {
    if old == new {
        return None;
    }
    let (Ok(old), Ok(new)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else {
        return Some("binary content differs\n".into());
    };
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let Some(ops) = ops(&a, &b) else {
        return Some("too large to show\n".into());
    };
    let mut out = format!("--- {path}\n+++ {path} (planned)\n");
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.0 != Op::Keep)
        .map(|(k, _)| k)
        .collect();
    if changed.is_empty() {
        out.push_str("(only the final newline differs)\n");
        return Some(out);
    }
    let mut k = 0;
    while k < changed.len() {
        let start = changed[k].saturating_sub(CONTEXT);
        let mut end = changed[k];
        while k + 1 < changed.len() && changed[k + 1] <= end + 2 * CONTEXT + 1 {
            k += 1;
            end = changed[k];
        }
        let end = (end + CONTEXT + 1).min(ops.len());
        let hunk = &ops[start..end];
        let a_start = hunk[0].1;
        let b_start = hunk[0].2;
        let a_len = hunk.iter().filter(|o| o.0 != Op::Add).count();
        let b_len = hunk.iter().filter(|o| o.0 != Op::Del).count();
        let shown = |start: usize, len: usize| if len == 0 { start } else { start + 1 };
        writeln!(
            out,
            "@@ -{},{a_len} +{},{b_len} @@",
            shown(a_start, a_len),
            shown(b_start, b_len)
        )
        .unwrap();
        for (op, i, j) in hunk {
            match op {
                Op::Keep => writeln!(out, " {}", a[*i]).unwrap(),
                Op::Del => writeln!(out, "-{}", a[*i]).unwrap(),
                Op::Add => writeln!(out, "+{}", b[*j]).unwrap(),
            }
        }
        k += 1;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_texts_have_no_diff() {
        assert_eq!(unified(b"a\n", b"a\n", "/etc/x"), None);
    }

    #[test]
    fn a_change_gets_a_hunk_with_context() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let new = "1\n2\n3\n4\nfive\n6\n7\n8\n";
        let diff = unified(old.as_bytes(), new.as_bytes(), "/etc/x").unwrap();
        assert_eq!(
            diff,
            "--- /etc/x\n+++ /etc/x (planned)\n@@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+five\n 6\n 7\n 8\n"
        );
    }

    #[test]
    fn creation_diffs_against_nothing() {
        let diff = unified(b"", b"a\nb\n", "/etc/new").unwrap();
        assert!(diff.contains("@@ -0,0 +1,2 @@\n+a\n+b\n"), "{diff}");
    }

    #[test]
    fn distant_changes_get_separate_hunks() {
        let old: String = (0..30).map(|i| format!("{i}\n")).collect();
        let new: String = (0..30)
            .map(|i| match i {
                2 => "two\n".to_string(),
                27 => "twenty-seven\n".to_string(),
                i => format!("{i}\n"),
            })
            .collect();
        let diff = unified(old.as_bytes(), new.as_bytes(), "f").unwrap();
        assert_eq!(diff.matches("@@ -").count(), 2, "{diff}");
    }

    #[test]
    fn binary_is_a_note() {
        assert_eq!(unified(&[0xff, 0], b"x", "f").unwrap(), "binary content differs\n");
    }
}
