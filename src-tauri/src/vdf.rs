//! Minimal reader for Valve's KeyValues text format (.acf / .vdf).
//! We only need flat lookups, so this tokenizes quoted strings and braces
//! and offers "first value for key" / "all values for key" helpers.

#[derive(Debug, PartialEq)]
enum Token {
    Str(String),
    Open,
    Close,
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let mut value = String::new();
                while let Some(ch) = chars.next() {
                    match ch {
                        '\\' => match chars.next() {
                            Some('n') => value.push('\n'),
                            Some('t') => value.push('\t'),
                            Some(other) => value.push(other),
                            None => break,
                        },
                        '"' => break,
                        _ => value.push(ch),
                    }
                }
                tokens.push(Token::Str(value));
            }
            '{' => tokens.push(Token::Open),
            '}' => tokens.push(Token::Close),
            '/' if chars.peek() == Some(&'/') => {
                // line comment
                for ch in chars.by_ref() {
                    if ch == '\n' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    tokens
}

/// All string values that directly follow `key` (case-insensitive), in document order.
pub fn values(text: &str, key: &str) -> Vec<String> {
    let tokens = tokenize(text);
    let mut out = Vec::new();
    for pair in tokens.windows(2) {
        if let (Token::Str(k), Token::Str(v)) = (&pair[0], &pair[1]) {
            if k.eq_ignore_ascii_case(key) {
                out.push(v.clone());
            }
        }
    }
    out
}

/// First string value that follows `key`.
pub fn value(text: &str, key: &str) -> Option<String> {
    values(text, key).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_manifest_fields() {
        let acf = r#""AppState"
{
    "appid"     "2406770"
    "name"      "Bodycam \"Deluxe\""
    "installdir"    "Bodycam"
    "buildid"   "16543210"
    "UserConfig" { "language" "english" }
}"#;
        assert_eq!(value(acf, "appid").as_deref(), Some("2406770"));
        assert_eq!(value(acf, "name").as_deref(), Some("Bodycam \"Deluxe\""));
        assert_eq!(value(acf, "BuildID").as_deref(), Some("16543210"));
        assert_eq!(value(acf, "missing"), None);
    }

    #[test]
    fn reads_library_paths() {
        let vdf = r#""libraryfolders" { "0" { "path" "C:\\Program Files (x86)\\Steam" } "1" { "path" "E:\\SteamLibrary" } }"#;
        assert_eq!(
            values(vdf, "path"),
            vec!["C:\\Program Files (x86)\\Steam".to_string(), "E:\\SteamLibrary".to_string()]
        );
    }
}
