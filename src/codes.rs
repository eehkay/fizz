//! Spotting one-time codes in messages: "Your verification code is 482913".
//!
//! This is guesswork by design, so it leans towards saying nothing: a
//! message has to talk about a code *and* contain a number shaped like one.

/// Words that mark a message as being about a code.
const HINTS: [&str; 16] = [
    "code",
    "otp",
    "passcode",
    "password",
    "verification",
    "verify",
    "one-time",
    "one time",
    "2fa",
    "two-factor",
    "security",
    "authentication",
    "login",
    "log in",
    "sign in",
    "pin",
];

/// The one-time code in a message, if it reads like one carries one.
/// Digits only: `123456`, and also `123 456`, `123-456` and `G-123456`.
pub fn find(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    if !HINTS.iter().any(|hint| lower.contains(hint)) {
        return None;
    }
    // Walk the text collecting runs of digits, where a run may hold single
    // spaces or hyphens between groups ("123 456").
    let chars: Vec<char> = text.chars().collect();
    let mut candidates: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut groups: Vec<String> = vec![String::new()];
        while i < chars.len() {
            let c = chars[i];
            if c.is_ascii_digit() {
                groups.last_mut().unwrap().push(c);
                i += 1;
            } else if (c == ' ' || c == '-')
                && chars.get(i + 1).is_some_and(char::is_ascii_digit)
                && groups.last().is_some_and(|g| g.len() == 3 || g.len() == 4)
            {
                groups.push(String::new());
                i += 1;
            } else {
                break;
            }
        }
        // What sits either side tells a code from a price, a date, a time,
        // a percentage or part of a longer reference.
        let before = start.checked_sub(1).map(|b| chars[b]);
        let after = chars.get(i).copied();
        let money = matches!(before, Some('$' | '£' | '€' | '#'));
        let part_of_more = matches!(before, Some('/' | ':' | '.' | ','))
            || matches!(after, Some('/' | ':' | '%'))
            || after.is_some_and(|c| c.is_alphanumeric())
            || (before.is_some_and(|c| c.is_alphabetic())
                // "G-123456" is fine; "order48291" is not.
                && !matches!(before, Some('-')));
        let decimal = after == Some('.') && chars.get(i + 1).is_some_and(char::is_ascii_digit);
        if money || part_of_more || decimal {
            continue;
        }
        let joined: String = groups.concat();
        // Two equal groups ("123 456", "1234-5678") are one code. Anything
        // else that came in several groups is a phone number or similar.
        let plausible_groups = match groups.as_slice() {
            [_] => true,
            [a, b] => a.len() == b.len(),
            _ => false,
        };
        if plausible_groups && (4..=8).contains(&joined.len()) {
            candidates.push(joined);
        } else if groups.len() == 2 && (4..=8).contains(&groups[0].len()) {
            // "482913 2026": unequal groups are separate numbers; the
            // first may still be the code.
            candidates.push(groups[0].clone());
        }
    }
    // Six digits is by far the commonest length; otherwise the first.
    candidates
        .iter()
        .find(|c| c.len() == 6)
        .or_else(|| candidates.first())
        // A bare four-digit year is not a code.
        .filter(|c| !(c.len() == 4 && (c.starts_with("19") || c.starts_with("20")) && candidates.len() == 1 && !lower.contains("pin") && !lower.contains("code")))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::find;

    #[test]
    fn finds_codes_in_common_wordings() {
        let cases = [
            ("Your verification code is 482913.", "482913"),
            ("G-123456 is your Google verification code.", "123456"),
            ("123 456 is your Facebook code", "123456"),
            ("Use 9374 to verify your account", "9374"),
            ("Your Chase code is 12345678. Don't share it.", "12345678"),
            ("482913 is your one-time passcode. It expires in 10 minutes.", "482913"),
            ("Your security code: 0042-1187", "00421187"),
            ("Amazon: Use 271828 to sign in. Do not share this OTP.", "271828"),
            ("Your login PIN is 0931", "0931"),
            ("<#> 554201 is your code. hA8xQz", "554201"),
            ("Code 771204. Call 800-555-0199 if this wasn't you.", "771204"),
        ];
        for (text, code) in cases {
            assert_eq!(find(text).as_deref(), Some(code), "{text}");
        }
    }

    #[test]
    fn leaves_ordinary_messages_alone() {
        let cases = [
            "See you at 7:15 tomorrow",
            "Your order 48291 has shipped",
            "The gate code hasn't changed, see you soon",
            "Call me at 800-555-0199 about the security deposit",
            "I paid $1234 for the security system",
            "Password reset requested on 11/19/2026",
            "My pin fell off the map",
            "The login page is down, 50% of users affected",
            "",
        ];
        for text in cases {
            assert_eq!(find(text), None, "{text}");
        }
    }
}
