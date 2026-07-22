//! Terminal output helpers.

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Shorten a manifest id for display: `myc1-ab12cd34…`.
pub fn short_id(id: &str) -> String {
    if let Some(hash) = id.strip_prefix("myc1-") {
        format!("myc1-{}", &hash[..12.min(hash.len())])
    } else {
        id.chars().take(17).collect()
    }
}

/// Minimal ANSI styling: honors NO_COLOR, TERM=dumb and non-tty stdout.
#[derive(Clone, Copy)]
pub struct Paint {
    on: bool,
}

impl Paint {
    pub fn auto() -> Self {
        use std::io::IsTerminal;
        let on = std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
            && std::io::stdout().is_terminal();
        Paint { on }
    }

    fn wrap(&self, s: &str, code: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn bold(&self, s: &str) -> String {
        self.wrap(s, "1")
    }
    pub fn dim(&self, s: &str) -> String {
        self.wrap(s, "2")
    }
    pub fn red(&self, s: &str) -> String {
        self.wrap(s, "31")
    }
    pub fn green(&self, s: &str) -> String {
        self.wrap(s, "32")
    }
    pub fn yellow(&self, s: &str) -> String {
        self.wrap(s, "33")
    }
    pub fn cyan(&self, s: &str) -> String {
        self.wrap(s, "36")
    }
}

/// Classic Levenshtein edit distance, used for "did you mean" suggestions.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Pick up to three close matches for `input` among `candidates`, closest
/// first. A candidate qualifies when its distance is small relative to the
/// input length (or when the input is a clear substring of it).
pub fn suggest(input: &str, candidates: &[String]) -> Vec<String> {
    let max_distance = (input.len() / 3).max(2);
    let mut scored: Vec<(usize, &String)> = candidates
        .iter()
        .filter_map(|c| {
            if c.contains(input) {
                return Some((1, c));
            }
            let d = levenshtein(input, c);
            (d <= max_distance).then_some((d, c))
        })
        .collect();
    scored.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(y.1)));
    scored.dedup_by(|x, y| x.1 == y.1);
    scored.into_iter().take(3).map(|(_, c)| c.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn ids() {
        assert_eq!(short_id("myc1-abcdef0123456789ff"), "myc1-abcdef012345");
    }

    #[test]
    fn edit_distance() {
        assert_eq!(levenshtein("alpine", "alpine"), 0);
        assert_eq!(levenshtein("alpine", "alpin"), 1);
        assert_eq!(levenshtein("alpine", "aplime"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn suggestions_rank_close_matches() {
        let candidates = vec![
            "alpine:3.20".to_string(),
            "alpine:3.21".to_string(),
            "debian:12".to_string(),
            "docker.io/library/alpine:3.20".to_string(),
        ];
        // Typo: distance 1 from alpine:3.20.
        let s = suggest("alpne:3.20", &candidates);
        assert_eq!(s[0], "alpine:3.20");
        // Substring matches qualify too.
        let s = suggest("alpine", &candidates);
        assert!(s.contains(&"alpine:3.20".to_string()));
        // Nothing close.
        assert!(suggest("postgres:16", &candidates).is_empty());
    }
}
