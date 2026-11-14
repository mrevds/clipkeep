//! "Is this clip a secret?" Secrets (passwords, API keys, private keys) are kept in memory only,
//! never written to disk or synced, and forgotten after `secret_seconds` (default 60).
//!
//! Two signals:
//! 1. The app that copied it says so. Password managers mark their copies: Windows
//!    `ExcludeClipboardContentFromMonitorProcessing`, macOS `org.nspasteboard.ConcealedType`,
//!    Linux `x-kde-passwordManagerHint` (KeePassXC, KeePass, Bitwarden, 1Password ...). That check
//!    lives in `daemon::platform::concealed`.
//! 2. The text looks like one (this module): well-known token formats, private keys, recovery
//!    codes, and random-looking single words with mixed character classes. Deliberately
//!    conservative: a false alarm only makes a clip disappear after a minute, but code, paths,
//!    links, e-mails, hashes and ordinary words must not trigger it.

/// Prefixes of API keys and tokens that are secret by definition.
const TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_", // GitHub
    "glpat-",      // GitLab
    "sk-",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "pk_live_", // OpenAI, Anthropic, Stripe
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-", // Slack
    "AKIA",
    "ASIA", // AWS access key ids
    "AIza", // Google API keys
    "GOCSPX-",
    "ya29.",
    "1//", // Google OAuth secret / tokens
    "npm_",
    "pypi-",
    "dop_v1_",
    "hf_",
    "glc_",
    "shpat_",
];

pub fn looks_secret(text: &str) -> bool {
    let t = text.trim();
    if t.contains("-----BEGIN") && t.contains("PRIVATE KEY-----") {
        return true;
    }
    let n = t.chars().count();
    if !(8..=200).contains(&n) || t.chars().any(char::is_whitespace) {
        return false;
    }
    if is_token(t) || is_jwt(t) || is_recovery_code(t) {
        return true;
    }
    n <= 64 && !is_ordinary(t) && is_password_like(t)
}

fn is_token(t: &str) -> bool {
    TOKEN_PREFIXES.iter().any(|p| {
        t.len() >= p.len() + 12
            && t.starts_with(p)
            && t[p.len()..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_./=".contains(&b))
    })
}

/// `xxxxx.yyyyy.zzzzz`, base64url parts, header starting with `{"` (`eyJ`).
fn is_jwt(t: &str) -> bool {
    let parts: Vec<&str> = t.split('.').collect();
    parts.len() == 3
        && t.starts_with("eyJ")
        && parts.iter().all(|p| {
            p.len() >= 10
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// Our own sync recovery code (and similar): 8+ groups of 4 base32 letters joined by '-'.
fn is_recovery_code(t: &str) -> bool {
    let groups: Vec<&str> = t.split('-').collect();
    groups.len() >= 8
        && groups.iter().all(|g| {
            g.len() == 4
                && g.bytes()
                    .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
        })
}

/// Things people copy all the time that may contain mixed characters but are not secrets.
fn is_ordinary(t: &str) -> bool {
    let lower = t.to_ascii_lowercase();
    let b = t.as_bytes();
    lower.contains("://")
        || lower.starts_with("www.")
        || t.starts_with('/')
        || t.starts_with('~')
        || t.starts_with("./")
        || t.contains('\\')
        || (t.contains('@') && t.rfind('.') > t.find('@')) // e-mail
        || b.iter().all(|c| c.is_ascii_hexdigit() || *c == b'-') // hashes, uuids, colors
        || b.iter().all(|c| c.is_ascii_digit() || b"+-.,:/()".contains(c)) // numbers, phones, dates
        || (t.contains('(') && t.contains(')')) // code: f(x), calls
        || has_file_extension(t)
        || !t.is_ascii()
}

fn has_file_extension(t: &str) -> bool {
    match t.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=4).contains(&ext.len())
                && ext
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && ext.bytes().any(|c| c.is_ascii_lowercase())
        }
        None => false,
    }
}

/// A single word that mixes character classes like a typed or generated password.
fn is_password_like(t: &str) -> bool {
    let class = |c: char| -> u8 {
        if c.is_ascii_lowercase() {
            0
        } else if c.is_ascii_uppercase() {
            1
        } else if c.is_ascii_digit() {
            2
        } else {
            3
        }
    };
    let chars: Vec<char> = t.chars().collect();
    let mut seen = [false; 4];
    let mut switches = 0;
    for (i, &c) in chars.iter().enumerate() {
        seen[class(c) as usize] = true;
        if i > 0 && class(c) != class(chars[i - 1]) {
            switches += 1;
        }
    }
    let [lower, upper, digit, symbol] = seen;
    let classes = seen.iter().filter(|s| **s).count();
    // A "strong" symbol: separators like - _ . are common in names, these are not.
    let strong = t.chars().any(|c| "!@#$%^&*+=?~|<>{}[];:'\",`".contains(c));
    let randomness = switches as f32 / (chars.len() - 1) as f32;
    if symbol {
        // Hunter2023!  P@ssw0rd-x  k9$Lm#2qWz
        strong && classes >= 3 && (upper || digit) && lower
    } else {
        // Generated alphanumeric passwords: long, all three classes, jumpy, several digits and
        // no long runs of lowercase letters. Identifiers (getUserName2, parseHtml5Document)
        // are made of words, so they have long lowercase runs.
        let digits = chars.iter().filter(|c| c.is_ascii_digit()).count();
        let mut run = 0;
        let mut longest_lower = 0;
        for c in &chars {
            run = if c.is_ascii_lowercase() { run + 1 } else { 0 };
            longest_lower = longest_lower.max(run);
        }
        chars.len() >= 12
            && lower
            && upper
            && digit
            && digits >= 2
            && longest_lower <= 4
            && randomness >= 0.35
    }
}

#[cfg(test)]
mod tests {
    use super::looks_secret;

    #[test]
    fn secrets_are_caught() {
        for s in [
            "Hunter2023!",
            "P@ssw0rd-2024",
            "k9$Lm#2qWz!x",
            "correct#Horse7battery",
            "x7Kp2Lm9Qw4ZbT6r",
            "aZ3kP9xQ2mW7vL5n",
            "ghp_1234567890abcdefghijABCDEFGHIJ123456",
            "sk-proj-AbCdEf1234567890xyzXYZ",
            "AKIAIOSFODNN7EXAMPLE",
            "GOCSPX-abcdefghijklmnopqrstuvwxyz12",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            "WXLS-MRG5-HX2N-YBKV-5XCQ-C5J4-6MJJ-SVPZ-R2MR-AO2B-QES6-2OHS-FZMA",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----",
            "  Hunter2023!\n",
        ] {
            assert!(looks_secret(s), "should be secret: {s:?}");
        }
    }

    #[test]
    fn ordinary_clips_are_not() {
        for s in [
            "hello",
            "hello world, how are you?",
            "Привет, это тест группы Работа",
            "пароль123!Ж",
            "https://example.com/a?b=1&c=Xy9",
            "www.Example1.com",
            "user.Name2@mail.com",
            "/home/user/Projects/App1",
            "C:\\Users\\Me\\File1.txt",
            "my-file_v2.txt",
            "Report_2024-Q3.pdf",
            "getUserName2",
            "HttpClientFactory2Builder",
            "parseHtml5Document",
            "d41d8cd98f00b204e9800998ecf8427e",
            "faa89818cab8f5fb1bcaac241a0ccdf2111fd8ff",
            "550e8400-e29b-41d4-a716-446655440000",
            "+998 (90) 123-45-67",
            "2026-11-14",
            "12345678",
            "foo(bar1);",
            "x+1",
            "rust-lang/rust",
            "feature/DROP-123",
            "#B3202D",
            "ABCDEFGHIJK",
            "Wednesday",
        ] {
            assert!(!looks_secret(s), "should not be secret: {s:?}");
        }
    }
}
