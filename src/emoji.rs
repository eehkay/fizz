//! Finding emoji by name, for the `:name` popup in the message box.

pub struct Match {
    pub emoji: &'static str,
    /// The name to show next to it, e.g. `thumbsup`.
    pub label: &'static str,
}

/// The emoji whose short name is exactly `name` (as in `:name:`).
pub fn exact(name: &str) -> Option<&'static str> {
    emojis::get_by_shortcode(name).map(|e| e.as_str())
}

/// Best matches for a partly typed name, most relevant first.
pub fn search(query: &str, limit: usize) -> Vec<Match> {
    let query = query.to_lowercase();
    // Lower is better: exact name, name starts with it, a word of the
    // description starts with it, it appears anywhere.
    let mut ranked: Vec<(u8, usize, Match)> = Vec::new();
    for (order, emoji) in emojis::iter().enumerate() {
        let mut best: Option<(u8, &'static str)> = None;
        for code in emoji.shortcodes() {
            let rank = if code == query {
                0
            } else if code.starts_with(&query) {
                1
            } else if code.split('_').any(|word| word.starts_with(&query)) {
                2
            } else if code.contains(&query) {
                4
            } else {
                continue;
            };
            if best.is_none_or(|(r, _)| rank < r) {
                best = Some((rank, code));
            }
        }
        if best.is_none_or(|(r, _)| r > 3) {
            let name = emoji.name();
            if name.split([' ', '-']).any(|word| word.starts_with(&query)) {
                best = Some((3, emoji.shortcode().unwrap_or(name)));
            }
        }
        if let Some((rank, label)) = best {
            ranked.push((
                rank,
                order,
                Match {
                    emoji: emoji.as_str(),
                    label,
                },
            ));
        }
    }
    // Loose "appears somewhere in the name" hits are only worth showing
    // when there is little else.
    if ranked.iter().filter(|(rank, ..)| *rank < 4).count() >= 3 {
        ranked.retain(|(rank, ..)| *rank < 4);
    }
    ranked.sort_by_key(|(rank, order, m)| (*rank, m.label.len().min(64), *order));
    ranked.into_iter().take(limit).map(|(_, _, m)| m).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_by_name() {
        assert_eq!(exact("thumbsup"), Some("👍"));
        assert_eq!(exact("nope_not_real"), None);
        let hits = search("thumbs", 5);
        assert!(hits.iter().any(|m| m.emoji == "👍"));
        assert_eq!(search("heart", 3)[0].emoji, "❤️");
        assert!(search("zzzzqq", 5).is_empty());
    }
}
