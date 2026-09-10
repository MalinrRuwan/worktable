//! Detected actions for an entry's text: links, email addresses, phone
//! numbers. Pure scanning (no regex crate) with tests, so the entry view can
//! offer an "Open link" / "Email" / "Call" chip when the content contains one.

/// One action the entry view can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryAction {
    Link(String),
    Email(String),
    Phone(String),
}

/// Detect at most one of each action kind in `content`.
pub fn detect_actions(content: &str) -> Vec<EntryAction> {
    let mut actions = Vec::new();
    let (mut has_link, mut has_email, mut has_phone) = (false, false, false);
    for raw in content.split_whitespace() {
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                '(' | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '<'
                    | '>'
                    | '"'
                    | '\''
                    | ','
                    | ';'
                    | '!'
                    | '?'
                    | ':'
                    | '.'
            )
        });
        if token.is_empty() {
            continue;
        }
        if !has_link && (token.starts_with("http://") || token.starts_with("https://")) {
            actions.push(EntryAction::Link(token.to_owned()));
            has_link = true;
            continue;
        }
        if !has_email && is_email(token) {
            actions.push(EntryAction::Email(token.to_owned()));
            has_email = true;
        }
    }
    // Phones may contain spaces ("+1 (415) 555-0123"), so scan runs of
    // phone-ish characters across the whole text instead of per token.
    if !has_phone && let Some(phone) = detect_phone(content) {
        actions.push(EntryAction::Phone(phone));
        has_phone = true;
    }
    let _ = has_phone;
    actions
}

/// Scan for the first phone-like run: digits and separators, 9–15 digits,
/// starting with a digit or `+`. Dates (8 digits) stay out.
fn detect_phone(content: &str) -> Option<String> {
    let mut candidate = String::new();
    let flush = |candidate: &mut String| -> Option<String> {
        let trimmed = candidate
            .trim_matches(|c: char| matches!(c, ' ' | '-' | '.' | '(' | ')'))
            .to_owned();
        candidate.clear();
        let digits = trimmed.chars().filter(|c| c.is_ascii_digit()).count();
        if (9..=15).contains(&digits) {
            Some(trimmed)
        } else {
            None
        }
    };
    for c in content.chars().chain(std::iter::once('\n')) {
        if c.is_ascii_digit() || matches!(c, '+' | '-' | '(' | ')' | '.' | ' ') {
            candidate.push(c);
        } else if let Some(phone) = flush(&mut candidate) {
            return Some(phone);
        }
    }
    None
}

fn is_email(token: &str) -> bool {
    let Some((user, host)) = token.split_once('@') else {
        return false;
    };
    if user.is_empty() || host.is_empty() {
        return false;
    }
    let user_ok = user.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '.' | '!'
                    | '#'
                    | '$'
                    | '%'
                    | '&'
                    | '\''
                    | '*'
                    | '+'
                    | '/'
                    | '='
                    | '?'
                    | '^'
                    | '_'
                    | '`'
                    | '{'
                    | '|'
                    | '}'
                    | '~'
                    | '-'
            )
    });
    let host_ok = host.contains('.')
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
    user_ok && host_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_links_emails_and_mobiles() {
        let actions = detect_actions(
            "Reach me at jane.doe+notes@example.com or +1 (415) 555-0123. \\
             Reference: https://example.com/papers/42.",
        );
        assert!(actions.contains(&EntryAction::Link(
            "https://example.com/papers/42".to_owned()
        )));
        assert!(actions.contains(&EntryAction::Email("jane.doe+notes@example.com".to_owned())));
        assert!(
            matches!(actions.as_slice(), [.., EntryAction::Phone(p)] if p.contains("555-0123")),
            "expected a phone action: {actions:?}"
        );
    }

    #[test]
    fn dates_are_not_phone_numbers() {
        let actions = detect_actions("captured on 1970-01-01 and 2026-09-10");
        assert!(
            !actions.iter().any(|a| matches!(a, EntryAction::Phone(_))),
            "dates must not offer a call chip: {actions:?}"
        );
    }

    #[test]
    fn plain_notes_detect_nothing() {
        assert!(detect_actions("just a note about configs").is_empty());
    }
}
