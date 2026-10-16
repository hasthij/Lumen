//! Fuzzy matching tuned for launcher queries: exact and prefix matches first, then word
//! starts and acronyms ("vsc" → Visual Studio Code), then loose subsequences.

/// Scores `query` (already lower-case) against `text`. Higher is better; `None` is no match.
/// Every whitespace-separated query word must match.
pub fn score(query: &str, text: &str) -> Option<i32> {
    let text = text.to_lowercase();
    let mut total = 0;
    let mut words = 0;
    for word in query.split_whitespace() {
        total += word_score(word, &text)?;
        words += 1;
    }
    if words == 0 {
        return None;
    }
    if text == query {
        total += 400;
    }
    // Shorter names win ties: "Code" over "Code Insiders".
    Some(total - (text.chars().count() as i32).min(60))
}

fn word_score(q: &str, text: &str) -> Option<i32> {
    if let Some(pos) = text.find(q) {
        let at_word_start = pos == 0 || !text[..pos].chars().last().is_some_and(char::is_alphanumeric);
        return Some(match (pos, at_word_start) {
            (0, _) => 1000,
            (_, true) => 850,
            _ => 600 - (pos as i32).min(200),
        });
    }

    let chars: Vec<char> = text.chars().collect();
    let starts: Vec<usize> =
        (0..chars.len()).filter(|&i| chars[i].is_alphanumeric() && (i == 0 || !chars[i - 1].is_alphanumeric())).collect();
    let initials: String = starts.iter().map(|&i| chars[i]).collect();
    if initials.starts_with(q) {
        return Some(800);
    }

    // Subsequence ("gchr" → Google Chrome): it must start at a word start, and every jump
    // must land on a word start, so random letters scattered through a name don't match.
    let mut score = 300;
    let mut ti = 0;
    let mut prev: Option<usize> = None;
    for qc in q.chars() {
        let consecutive = prev.map(|p| p + 1);
        let found = match consecutive.filter(|&c| chars.get(c) == Some(&qc)) {
            Some(c) => {
                score += 15;
                c
            }
            None => {
                let f = starts.iter().copied().find(|&i| i >= ti && chars[i] == qc)?;
                score -= 10;
                f
            }
        };
        prev = Some(found);
        ti = found + 1;
    }
    (q.chars().count() >= 2).then_some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranking() {
        let s = |q: &str, t: &str| score(q, t).unwrap_or(i32::MIN);
        assert!(s("code", "Visual Studio Code") > s("code", "Encoder"));
        assert!(s("vsc", "Visual Studio Code") > 0);
        assert!(s("chr", "Google Chrome") > 0);
        assert!(s("note", "Notepad") > s("note", "OneNote"));
        assert!(s("word", "Word") > s("word", "WordPad"));
        assert_eq!(score("xyz", "Notepad"), None);
        assert!(s("reg edit", "Registry Editor") > 0);
        assert!(s("gchr", "Google Chrome") > 0);
        assert_eq!(score("lumen", "Volume Activation Tools"), None);
        assert_eq!(score("lumen", "Splice INSTRUMENT"), None);
    }
}
