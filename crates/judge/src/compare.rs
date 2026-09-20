//! Output comparison. Runs on the host, outside the sandbox, so user code never sees answers.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparer {
    /// Line by line; trailing whitespace on each line and trailing blank lines are ignored.
    #[default]
    Lines,
    /// Whitespace-separated tokens must match exactly; layout is ignored.
    Tokens,
    /// Like `Tokens`, but tokens that parse as numbers match within an
    /// absolute or relative tolerance.
    Float { eps: f64 },
}

/// `Ok(())` on a match, otherwise a short explanation safe to show to users
/// (it never contains expected output).
pub fn compare(comparer: Comparer, expected: &[u8], actual: &[u8]) -> Result<(), String> {
    match comparer {
        Comparer::Lines => compare_lines(expected, actual),
        Comparer::Tokens => compare_tokens(expected, actual, None),
        Comparer::Float { eps } => compare_tokens(expected, actual, Some(eps)),
    }
}

fn normalized_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = data
        .split(|&b| b == b'\n')
        .map(|line| {
            let end = line.iter().rposition(|b| !matches!(b, b' ' | b'\t' | b'\r')).map_or(0, |i| i + 1);
            &line[..end]
        })
        .collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

fn compare_lines(expected: &[u8], actual: &[u8]) -> Result<(), String> {
    let e = normalized_lines(expected);
    let a = normalized_lines(actual);
    for (i, (el, al)) in e.iter().zip(a.iter()).enumerate() {
        if el != al {
            return Err(format!("line {} differs", i + 1));
        }
    }
    if e.len() != a.len() {
        return Err(format!("expected {} lines, got {}", e.len(), a.len()));
    }
    Ok(())
}

fn tokens(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.split(|b| b.is_ascii_whitespace()).filter(|t| !t.is_empty())
}

fn compare_tokens(expected: &[u8], actual: &[u8], eps: Option<f64>) -> Result<(), String> {
    let mut e = tokens(expected);
    let mut a = tokens(actual);
    let mut index = 0usize;
    loop {
        index += 1;
        match (e.next(), a.next()) {
            (None, None) => return Ok(()),
            (Some(_), None) => return Err(format!("output ended early at token {index}")),
            (None, Some(_)) => return Err(format!("unexpected extra token {index}")),
            (Some(et), Some(at)) => {
                if et == at {
                    continue;
                }
                if let Some(eps) = eps {
                    if floats_match(et, at, eps) {
                        continue;
                    }
                }
                return Err(format!("token {index} differs"));
            }
        }
    }
}

fn floats_match(expected: &[u8], actual: &[u8], eps: f64) -> bool {
    let parse = |t: &[u8]| std::str::from_utf8(t).ok()?.parse::<f64>().ok().filter(|v| v.is_finite());
    match (parse(expected), parse(actual)) {
        (Some(e), Some(a)) => {
            let diff = (e - a).abs();
            diff <= eps || diff <= eps * e.abs()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_ignore_trailing_whitespace() {
        assert!(compare(Comparer::Lines, b"1 2\n3\n", b"1 2  \r\n3").is_ok());
        assert!(compare(Comparer::Lines, b"1 2\n3\n", b"1 2\n3\n\n\n").is_ok());
        assert!(compare(Comparer::Lines, b"", b"\n").is_ok());
    }

    #[test]
    fn lines_detect_differences() {
        assert_eq!(compare(Comparer::Lines, b"1\n2\n", b"1\n3\n").unwrap_err(), "line 2 differs");
        assert!(compare(Comparer::Lines, b"1\n2\n", b"1\n").is_err());
        assert!(compare(Comparer::Lines, b"1 2\n", b"1  2\n").is_err(), "inner whitespace matters");
        assert!(compare(Comparer::Lines, b" 1\n", b"1\n").is_err(), "leading whitespace matters");
    }

    #[test]
    fn tokens_ignore_layout() {
        assert!(compare(Comparer::Tokens, b"1 2\n3\n", b"1\n2   3").is_ok());
        assert!(compare(Comparer::Tokens, b"1 2 3", b"1 2").is_err());
        assert!(compare(Comparer::Tokens, b"1 2", b"1 2 3").is_err());
        assert!(compare(Comparer::Tokens, b"abc", b"abd").is_err());
    }

    #[test]
    fn float_tolerance() {
        let c = Comparer::Float { eps: 1e-6 };
        assert!(compare(c, b"0.3333333", b"0.33333334").is_ok());
        assert!(compare(c, b"1000000.0", b"1000000.5").is_ok(), "relative tolerance");
        assert!(compare(c, b"0.5", b"0.6").is_err());
        assert!(compare(c, b"yes 1.0", b"yes 1.0000001").is_ok());
        assert!(compare(c, b"yes", b"no").is_err());
        assert!(compare(c, b"1.0", b"nan").is_err());
    }
}
