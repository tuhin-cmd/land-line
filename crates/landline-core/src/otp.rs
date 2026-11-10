//! One-time codes in message text.
//!
//! The rules favour precision over recall: a missed code costs the user one
//! manual copy, while a wrong "code" pasted into a login form is worse. So a
//! number is only taken as a code when the text says it is one (a keyword
//! such as "code", "OTP" or "ওটিপি"), when it is shaped like one (4 to 8
//! digits, or two near-equal groups split by one space or hyphen), and when
//! nothing around it says it is something else: an amount, a date or time,
//! a year, or part of a phone, account, card or reference number.

/// A one-time code found in a message or notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtpCode {
    /// The digits, without the spaces or hyphens some senders add.
    pub code: String,
    /// The app or sender the text came from.
    pub source: String,
}

/// The one-time code in `text`, if it carries one: ASCII digits only, with
/// Bangla digits converted and any split ("123 456") joined.
pub fn extract(text: &str) -> Option<String> {
    let chars = normalize(text);
    let keywords = find_keywords(&chars);
    if keywords.is_empty() {
        return None;
    }

    let mut best: Option<(usize, String)> = None;
    for candidate in candidates(&chars) {
        let Some(gap) = keywords.iter().map(|k| gap(k, &candidate.span)).min() else {
            continue;
        };
        // Senders put the code next to the keyword ("code is 123456",
        // "123456 is your OTP"); a number far from any keyword is about
        // something else.
        if gap > MAX_GAP {
            continue;
        }
        // "2026" is far more often a year than a code, so it only counts as
        // one when the keyword is right there, and any other candidate wins.
        let score = if candidate.year_like {
            if gap > YEAR_MAX_GAP {
                continue;
            }
            gap + YEAR_PENALTY
        } else {
            gap
        };
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score < *best_score)
        {
            best = Some((score, candidate.code));
        }
    }
    best.map(|(_, code)| code)
}

/// Characters between a keyword and a code, beyond which they are unrelated.
const MAX_GAP: usize = 60;
/// How close a year-like number must be to a keyword to count.
const YEAR_MAX_GAP: usize = 10;
/// Makes any other candidate beat a year-like one.
const YEAR_PENALTY: usize = 1_000;

/// Whole words, matched case-insensitively. "security code" and "one-time
/// password" are covered by their last word.
const LATIN_KEYWORDS: &[&str] = &[
    "code",
    "otp",
    "verification",
    "verify",
    "pin",
    "passcode",
    "password",
    "authentication",
];

/// A keyword right after one of these names a code that is not a login
/// code: "promo code 5050", "postal code 1207".
const NOT_OTP_QUALIFIERS: &[&str] = &[
    "promo",
    "promotional",
    "coupon",
    "voucher",
    "discount",
    "referral",
    "gift",
    "offer",
    "zip",
    "postal",
    "post",
    "area",
    "country",
    "ussd",
    "dial",
    "short",
    "qr",
];

/// Bangla keywords: code, OTP, PIN, verification, password, verify. Bangla
/// attaches suffixes ("কোডটি", "ওটিপিটি"), so these match as prefixes of a
/// word rather than whole words.
const BANGLA_KEYWORDS: &[&str] = &["কোড", "ওটিপি", "পিন", "ভেরিফিকেশন", "পাসওয়ার্ড", "যাচাই"];

/// A word right before a number that makes it an amount.
const CURRENCY_WORDS: &[&str] = &[
    "tk",
    "bdt",
    "taka",
    "usd",
    "rs",
    "inr",
    "eur",
    "gbp",
    "টাকা",
    "টাঃ",
];

const CURRENCY_SYMBOLS: &[char] = &['$', '৳', '₹', '€', '£', '¥', '₨'];

/// A word right before a number that makes it an identifier rather than a
/// code: account, card, reference and phone numbers, and the like.
const IDENTIFIER_WORDS: &[&str] = &[
    "a/c",
    "ac",
    "acc",
    "acct",
    "account",
    "card",
    "ending",
    "ref",
    "reference",
    "txn",
    "txnid",
    "trx",
    "trxid",
    "transaction",
    "id",
    "call",
    "phone",
    "mobile",
    "cell",
    "tel",
    "telephone",
    "hotline",
    "helpline",
    "help",
    "support",
    "contact",
    "dial",
    "ext",
    "invoice",
    "order",
    "tracking",
    "ticket",
    "booking",
    "pnr",
    "flight",
    "meter",
    "nid",
    "customer",
    "policy",
    "loan",
    "bill",
    "receipt",
    "হিসাব",
    "অ্যাকাউন্ট",
    "কার্ড",
    "কল",
];

/// Words that may sit between an identifier word and its number: "card
/// ending in 1234", "A/C no. 1234", "account number 1234".
const IDENTIFIER_CONNECTORS: &[&str] = &["no", "number", "num", "in", "with", "is"];

/// Abbreviations that end in a full stop ("Tk. 500", "A/C No. 1234"). Any
/// other word followed by a full stop ends a sentence, and says nothing
/// about the number that starts the next one.
const ABBREVIATIONS: &[&str] = &["tk", "rs", "no", "ac", "a/c", "acct", "ref", "tel", "txn"];

/// A word right after a number that makes it a quantity of something.
const UNIT_WORDS: &[&str] = &[
    "tk",
    "bdt",
    "taka",
    "usd",
    "rs",
    "inr",
    "টাকা",
    "min",
    "mins",
    "minute",
    "minutes",
    "hr",
    "hrs",
    "hour",
    "hours",
    "sec",
    "secs",
    "second",
    "seconds",
    "day",
    "days",
    "mb",
    "gb",
    "kb",
    "times",
    "pts",
    "points",
    "মিনিট",
    "সেকেন্ড",
];

/// Next to a number, these make it part of a date. "May" is left out: "1234
/// may be used" is a sentence, not a date.
const MONTHS: &[&str] = &[
    "jan",
    "january",
    "feb",
    "february",
    "mar",
    "march",
    "apr",
    "april",
    "jun",
    "june",
    "jul",
    "july",
    "aug",
    "august",
    "sep",
    "sept",
    "september",
    "oct",
    "october",
    "nov",
    "november",
    "dec",
    "december",
];

/// Characters that cannot touch a code on its left: masks ("****1234"),
/// phone prefixes, decimals, references ("#1234"), URLs and the like.
const REJECT_LEFT: &[char] = &[
    '*', '•', '#', '+', '/', '\\', '.', '_', '@', '%', '=', '&', '?', '~',
];

/// Characters that cannot touch a code on its right: percentages, "500/-",
/// masks and the like.
const REJECT_RIGHT: &[char] = &['*', '•', '/', '\\', '%', '@', '+', '_', '~'];

/// A run of digits joined by one of these to more digits is one number:
/// "5,000.00", "12/05/2026", "10:45", "123 456", "01712-345678".
fn is_joiner(c: char) -> bool {
    matches!(c, '.' | ',' | '/' | ':') || is_space_joiner(c) || is_dash(c)
}

fn is_space_joiner(c: char) -> bool {
    matches!(c, ' ' | '\u{00A0}' | '\u{2009}' | '\u{202F}')
}

fn is_dash(c: char) -> bool {
    matches!(c, '-' | '\u{2010}' | '\u{2011}' | '\u{2013}')
}

fn is_bangla(c: char) -> bool {
    ('\u{0980}'..='\u{09FF}').contains(&c)
}

/// A letter in any script, including Bangla vowel signs and the virama,
/// which the Unicode alphabetic property partly leaves out. Bangla digits
/// are converted before this is asked, and the taka signs are currency.
fn is_letter(c: char) -> bool {
    if is_bangla(c) {
        return !matches!(c, '৲' | '৳');
    }
    c.is_alphabetic()
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_digit() || is_letter(c)
}

/// Converts Bangla digits to ASCII and decomposes the few Bangla letters
/// that have two encodings, so keywords match however the sender typed them.
fn normalize(text: &str) -> Vec<char> {
    let mut chars = Vec::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '০'..='৯' => {
                let digit = c as u32 - '০' as u32;
                chars.push(char::from(b'0' + digit as u8));
            }
            '\u{09CB}' => chars.extend(['\u{09C7}', '\u{09BE}']),
            '\u{09CC}' => chars.extend(['\u{09C7}', '\u{09D7}']),
            '\u{09DC}' => chars.extend(['\u{09A1}', '\u{09BC}']),
            '\u{09DD}' => chars.extend(['\u{09A2}', '\u{09BC}']),
            '\u{09DF}' => chars.extend(['\u{09AF}', '\u{09BC}']),
            c => chars.push(c),
        }
    }
    chars
}

/// Whether `word` (already lowercased and normalized) is one of `list`.
fn listed(list: &[&str], word: &str) -> bool {
    list.iter()
        .any(|entry| *entry == word || normalize(entry).iter().copied().eq(word.chars()))
}

type Span = std::ops::Range<usize>;

/// Characters between two spans; 0 if they touch or overlap.
fn gap(a: &Span, b: &Span) -> usize {
    if a.end <= b.start {
        b.start - a.end
    } else {
        a.start.saturating_sub(b.end)
    }
}

fn find_keywords(chars: &[char]) -> Vec<Span> {
    let mut found = Vec::new();

    // Latin keywords: maximal runs of ASCII letters, so "pin" never
    // matches inside "shopping" or "spinning".
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            i += 1;
        }
        let touches_letter = (start > 0 && is_letter(chars[start - 1]))
            || chars.get(i).is_some_and(|c| is_letter(*c));
        if touches_letter {
            continue;
        }
        let word: String = chars[start..i]
            .iter()
            .map(char::to_ascii_lowercase)
            .collect();
        if !LATIN_KEYWORDS.contains(&word.as_str()) {
            continue;
        }
        let qualified = words_before(chars, start, 1)
            .first()
            .is_some_and(|w| NOT_OTP_QUALIFIERS.contains(&w.as_str()));
        if !qualified {
            found.push(start..i);
        }
    }

    for keyword in BANGLA_KEYWORDS {
        let keyword = normalize(keyword);
        for start in 0..chars.len() {
            let end = start + keyword.len();
            if chars.get(start..end) == Some(&keyword[..])
                && (start == 0 || !is_letter(chars[start - 1]))
            {
                found.push(start..end);
            }
        }
    }
    found
}

/// Up to `count` lowercased words before `pos`, nearest first. Stops at a
/// sentence boundary. "a/c" counts as one word.
fn words_before(chars: &[char], pos: usize, count: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut i = pos;
    while words.len() < count {
        // Skip what may sit between a word and a number: spaces and the
        // punctuation of "Code: 1234", "Ref#1234", "A/C-1234".
        let mut saw_full_stop = false;
        while i > 0 {
            let c = chars[i - 1];
            if c.is_whitespace() || matches!(c, ':' | '#' | '=' | ',' | '(' | ')') || is_dash(c) {
                i -= 1;
            } else if c == '.' && !saw_full_stop {
                saw_full_stop = true;
                i -= 1;
            } else {
                break;
            }
        }
        let end = i;
        while i > 0 {
            let c = chars[i - 1];
            let slash_in_word =
                c == '/' && i >= 2 && is_letter(chars[i - 2]) && end > i && is_letter(chars[i]);
            if is_word_char(c) || slash_in_word {
                i -= 1;
            } else {
                break;
            }
        }
        if i == end {
            break;
        }
        let word: String = chars[i..end]
            .iter()
            .flat_map(|c| c.to_lowercase())
            .collect();
        if saw_full_stop && !ABBREVIATIONS.contains(&word.as_str()) {
            break;
        }
        words.push(word);
    }
    words
}

/// The lowercased word right after `pos`, across spaces only.
fn word_after(chars: &[char], pos: usize) -> Option<String> {
    let mut i = pos;
    while i < chars.len() && is_space_joiner(chars[i]) {
        i += 1;
    }
    let start = i;
    while i < chars.len() && is_word_char(chars[i]) {
        i += 1;
    }
    (i > start).then(|| {
        chars[start..i]
            .iter()
            .flat_map(|c| c.to_lowercase())
            .collect()
    })
}

/// The first character before `pos`, or from `pos` on, that is not a space.
fn non_space_before(chars: &[char], pos: usize) -> Option<char> {
    chars[..pos]
        .iter()
        .rev()
        .copied()
        .find(|c| !c.is_whitespace())
}

fn non_space_from(chars: &[char], pos: usize) -> Option<char> {
    chars[pos..].iter().copied().find(|c| !c.is_whitespace())
}

struct Candidate {
    span: Span,
    code: String,
    year_like: bool,
}

/// Every number in `chars` that is shaped like a code and is not plainly
/// something else. Which one is the code is decided by keyword distance.
fn candidates(chars: &[char]) -> Vec<Candidate> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        // Read the whole number, joiners included, so that the parts of
        // "5,000.00" or "12/05/2026" are never taken on their own.
        let start = i;
        let mut groups = Vec::new();
        let mut joiners = Vec::new();
        loop {
            let group_start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            groups.push(group_start..i);
            let joined =
                i + 1 < chars.len() && is_joiner(chars[i]) && chars[i + 1].is_ascii_digit();
            if !joined {
                break;
            }
            joiners.push(chars[i]);
            i += 1;
        }
        let span = start..i;
        if let Some(code) = code_shape(chars, &groups, &joiners)
            && plausible(chars, &span)
        {
            let year_like = groups.len() == 1
                && code.len() == 4
                && code
                    .parse::<u32>()
                    .is_ok_and(|y| (1900..=2099).contains(&y));
            found.push(Candidate {
                span,
                code,
                year_like,
            });
        }
    }
    found
}

/// The digits, if the number is shaped like a code: one run of 4 to 8
/// digits, or two near-equal groups of 3 or 4 split by one space or hyphen
/// ("123 456", "1234-5678"). Anything else is an amount, a date, a time, a
/// phone number, or several numbers in a row.
fn code_shape(chars: &[char], groups: &[Span], joiners: &[char]) -> Option<String> {
    let digits = |span: &Span| chars[span.clone()].iter().collect::<String>();
    match (groups, joiners) {
        ([only], []) if (4..=8).contains(&only.len()) => Some(digits(only)),
        ([first, second], [joiner])
            if (is_space_joiner(*joiner) || is_dash(*joiner))
                && (3..=4).contains(&first.len())
                && (3..=4).contains(&second.len())
                && first.len().abs_diff(second.len()) <= 1 =>
        {
            Some(digits(first) + &digits(second))
        }
        _ => None,
    }
}

/// Whether the number at `span` could be a code, judging by its neighbours.
fn plausible(chars: &[char], span: &Span) -> bool {
    if let Some(&left) = span.start.checked_sub(1).and_then(|i| chars.get(i))
        && (is_letter(left) || REJECT_LEFT.contains(&left))
    {
        return false;
    }
    if let Some(&right) = chars.get(span.end)
        && (is_letter(right) || REJECT_RIGHT.contains(&right))
    {
        return false;
    }
    if non_space_before(chars, span.start).is_some_and(|c| CURRENCY_SYMBOLS.contains(&c))
        || non_space_from(chars, span.end).is_some_and(|c| CURRENCY_SYMBOLS.contains(&c))
    {
        return false;
    }

    let before = words_before(chars, span.start, 2);
    if let Some(w1) = before.first() {
        // A mask such as "XXXX-1234" or "xx 1234".
        let masked = w1.len() >= 2 && w1.chars().all(|c| c == 'x');
        if masked
            || listed(CURRENCY_WORDS, w1)
            || listed(IDENTIFIER_WORDS, w1)
            || MONTHS.contains(&w1.as_str())
        {
            return false;
        }
        if IDENTIFIER_CONNECTORS.contains(&w1.as_str())
            && before.get(1).is_some_and(|w2| listed(IDENTIFIER_WORDS, w2))
        {
            return false;
        }
    }
    if let Some(next) = word_after(chars, span.end)
        && (listed(UNIT_WORDS, &next) || MONTHS.contains(&next.as_str()))
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invented messages. None of the codes, numbers or names are real.
    const CASES: &[(&str, Option<&str>)] = &[
        // Plain English formats.
        ("Your verification code is 123456", Some("123456")),
        ("123456 is your OTP", Some("123456")),
        (
            "G-123456 is your Example verification code.",
            Some("123456"),
        ),
        ("Use 1234 as your login code. Never share it", Some("1234")),
        ("<#> Your code is 482913 AbCdEfGhIjK", Some("482913")),
        (
            "Your Examplepay verification code is 123456. It expires in 2 minutes",
            Some("123456"),
        ),
        ("Your one-time password is 90817263", Some("90817263")),
        ("Use passcode 5839 to sign in", Some("5839")),
        ("PIN: 7391 for your new card", Some("7391")),
        (
            "Security code 662145. Do not share it with anyone.",
            Some("662145"),
        ),
        ("Authentication code: 304918", Some("304918")),
        ("Verify your number with 4417", Some("4417")),
        ("Enter 381947 to verify your phone", Some("381947")),
        ("OTP:774411", Some("774411")),
        ("YOUR OTP IS 640218", Some("640218")),
        ("Your code is\n552019", Some("552019")),
        // Split codes.
        ("Your code is 123 456", Some("123456")),
        ("Your code is 123-456", Some("123456")),
        ("OTP: 4821-9037", Some("48219037")),
        ("Your Example code is 482 9137", Some("4829137")),
        // A code among other numbers.
        (
            "Your OTP for payment of Tk 1,250.00 is 845120. Valid for 5 minutes.",
            Some("845120"),
        ),
        (
            "Dear customer, use OTP 556677 to confirm transfer of BDT 5000 to A/C ***4321",
            Some("556677"),
        ),
        ("Your code is 482913. Call 16247 for help", Some("482913")),
        ("OTP 739104 for login on 10 Nov 2026", Some("739104")),
        ("Code 4821, valid till 10:45", Some("4821")),
        (
            "Example: 991827 is your security code. Don't share it. Ref 77001234",
            Some("991827"),
        ),
        (
            "Order 55120934 is on its way. Your delivery PIN is 3305",
            Some("3305"),
        ),
        ("Your login code is 2024", Some("2024")),
        // Bangla.
        ("আপনার ওটিপি ১২৩৪৫৬", Some("123456")),
        ("৪৮২৯১৩ আপনার ভেরিফিকেশন কোড", Some("482913")),
        ("আপনার পিন ৭৩৯১, কাউকে বলবেন না", Some("7391")),
        ("আপনার কোডটি হলো ৬৬০৪১২। কাউকে জানাবেন না।", Some("660412")),
        ("Your verification code is ১২৩ ৪৫৬", Some("123456")),
        (
            "প্রিয় গ্রাহক, আপনার ওটিপি ৮৮১২০৩। ৳৫০০ লেনদেনের জন্য।",
            Some("881203"),
        ),
        // No keyword, so no code, whatever the numbers look like.
        (
            "A/C ***1234 credited with BDT 15,000.00 on 10/11/26. Balance BDT 45,230.50",
            None,
        ),
        ("Your shopping total is 4500", None),
        ("Spinning class at 4821 Example Road", None),
        ("Meeting moved to 10:45 on 12/05/2026", None),
        ("Your parcel 48291734 has shipped", None),
        // A keyword, but only amounts, dates, identifiers or other shapes.
        ("Your PIN was used. Tk 5,000.00 withdrawn", None),
        ("OTP: 12 34 56", None),
        ("Your PIN for card ending 4321 was changed", None),
        ("Your PIN for card ending in 4321 was changed", None),
        (
            "Your password was changed. If this wasn't you, call 01712345678",
            None,
        ),
        (
            "Your verification code expired. Requested on 12/05/2026 10:45",
            None,
        ),
        ("Use promo code 5050 for 20% off", None),
        ("OTP sent to +8801712345678", None),
        ("Your OTP is valid for 10 minutes", None),
        (
            "Your PIN was reset in March 2026. Contact support for help.",
            None,
        ),
        ("Your password expires in 2026", None),
        ("Code ABC123456", None),
        ("Recharge Tk 1500 successful. Keep your PIN safe.", None),
        ("Your code is 1234567890", None),
        ("Your OTP has been used. Transaction ID 99887766", None),
        ("Verification pending for invoice 556677", None),
        ("Your code: ৳ 5000", None),
        ("Your verification code is 123-4567-89", None),
        ("Your PIN: 12-34", None),
        ("Your verification code is 48 2913", None),
        (
            "Bill of 1500/- paid. Use your PIN to confirm next time",
            None,
        ),
        ("Account 12345678 verification complete", None),
        ("A/C no. 12345678 verification done", None),
        ("Pay Rs.4500 using your UPI PIN", None),
        ("$1234 charged. Verify at example.com", None),
        ("Your code is 4821 2934 1928", None),
        ("Verify at https://example.com/v/482913", None),
        ("Your Example order #482913: verification required", None),
        ("Your code 1234% used", None),
        ("আপনার অ্যাকাউন্টে ৫০০০ টাকা জমা হয়েছে। পিন কাউকে বলবেন না", None),
        ("আপনার পিন পরিবর্তন হয়েছে। হেল্পলাইন: কল ১৬২৪৭", None),
    ];

    #[test]
    fn extracts_codes_and_rejects_lookalikes() {
        let mut failures = Vec::new();
        for (text, expected) in CASES {
            let got = extract(text);
            if got.as_deref() != *expected {
                failures.push(format!("{text:?}: expected {expected:?}, got {got:?}"));
            }
        }
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    #[test]
    fn has_a_broad_table() {
        let positives = CASES.iter().filter(|(_, code)| code.is_some()).count();
        let negatives = CASES.len() - positives;
        assert!(positives >= 20 && negatives >= 20);
    }

    #[test]
    fn keywords_are_whole_words() {
        assert_eq!(extract("SHOPPING 4821"), None);
        assert_eq!(extract("unpinned 4821"), None);
        assert_eq!(extract("decoded 4821"), None);
        assert_eq!(extract("PIN 4821"), Some("4821".into()));
    }

    #[test]
    fn takes_the_number_nearest_a_keyword() {
        assert_eq!(
            extract("Ticket 5512 confirmed. 7788 is your code"),
            Some("7788".into())
        );
        assert_eq!(
            extract("Your code is 7788, and 5512 is irrelevant here"),
            Some("7788".into())
        );
    }

    #[test]
    fn prefers_a_real_code_to_a_year() {
        assert_eq!(
            extract("2026 code: 482913"),
            Some("482913".into()),
            "a year next to the keyword must not beat a code"
        );
    }

    #[test]
    fn bangla_letters_are_matched_in_either_encoding() {
        // "কোড" with the vowel sign typed as two code points.
        assert_eq!(extract("আপনার ক\u{09C7}\u{09BE}ড ১২৩৪"), Some("1234".into()));
    }
}
