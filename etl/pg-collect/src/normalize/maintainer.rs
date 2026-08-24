//! Shared mailbox parser for Debian Maintainer / Uploaders fields.
//!
//! Handles RFC 5322-style mailbox-list parsing including:
//! - `Name <email>` (single maintainer)
//! - `Name1 <email1>, Name2 <email2>` (comma-separated co-maintainers)
//! - `"Doe, Jane" <jane@example.org>` (quoted display names with internal commas)
//! - `user@example.org` (bare addr-spec)
//! - `Debian QA Group` (name-only, no email)

/// A parsed mailbox entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    pub name: String,
    pub email: Option<String>,
}

/// Result of parsing a mailbox-list field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxParseResult {
    pub mailboxes: Vec<Mailbox>,
    pub malformed_count: usize,
}

/// Parse a comma-separated mailbox-list field (Maintainer or Uploaders).
///
/// Commas inside double-quoted display names are NOT treated as separators.
/// Each entry is classified as:
/// - `Name <email>` -> Mailbox { name, email: Some(...) }
/// - `user@domain.tld` (bare) -> Mailbox { name: addr, email: Some(addr) }
/// - `Name Only` (no email) -> Mailbox { name, email: None }
/// - Malformed (unmatched brackets, empty `<>`, etc.) -> increments malformed_count
pub fn parse_mailbox_list(input: &str) -> MailboxParseResult {
    let input = input.trim();
    if input.is_empty() {
        return MailboxParseResult {
            mailboxes: vec![],
            malformed_count: 1,
        };
    }

    let entries = split_mailbox_list(input);
    let mut mailboxes = Vec::new();
    let mut malformed_count = 0;

    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match parse_single_mailbox(entry) {
            SingleParseResult::Valid(m) => mailboxes.push(m),
            SingleParseResult::Malformed => malformed_count += 1,
        }
    }

    MailboxParseResult {
        mailboxes,
        malformed_count,
    }
}

/// Validate that an email address is safe for use in a URI.
///
/// Rejects emails containing IRI-unsafe characters (spaces, angle brackets,
/// control characters). Returns true if safe.
pub fn is_email_iri_safe(email: &str) -> bool {
    if email.is_empty() || !email.is_ascii() || !email.contains('@') {
        return false;
    }
    let bytes = email.as_bytes();
    let len = bytes.len();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b' ' | b'<' | b'>' | b'{' | b'}' | b'|' | b'\\' | b'^' | b'`' | b'"' => {
                return false;
            }
            b'%' => {
                // Must be followed by exactly 2 hex digits for valid percent-encoding
                if i + 2 >= len
                    || !bytes[i + 1].is_ascii_hexdigit()
                    || !bytes[i + 2].is_ascii_hexdigit()
                {
                    return false;
                }
            }
            _ if b < 0x20 => return false, // reject control chars
            _ => {}
        }
    }
    true
}

enum SingleParseResult {
    Valid(Mailbox),
    Malformed,
}

/// Parse a single mailbox entry (already split from the comma-separated list).
fn parse_single_mailbox(entry: &str) -> SingleParseResult {
    let entry = entry.trim();

    // Check for angle-bracket email: "Name <email>" or "<email>"
    if let Some(lt_pos) = entry.find('<') {
        let gt_pos = match entry.find('>') {
            Some(pos) if pos > lt_pos => pos,
            // Unmatched `<` -> malformed
            _ => return SingleParseResult::Malformed,
        };

        let email = entry[lt_pos + 1..gt_pos].trim();
        if email.is_empty() {
            // `<>` or `Name <>` -> malformed
            return SingleParseResult::Malformed;
        }

        // Extract display name (everything before `<`, stripping quotes)
        let name_part = entry[..lt_pos].trim();
        let name = strip_quotes(name_part);
        let display_name = if name.is_empty() {
            // `<user@example.org>` with no display name
            email.to_string()
        } else {
            name.to_string()
        };

        SingleParseResult::Valid(Mailbox {
            name: display_name,
            email: Some(email.to_string()),
        })
    } else if entry.contains('>') {
        // Stray `>` without `<` -> malformed
        SingleParseResult::Malformed
    } else if looks_like_email(entry) {
        // Bare addr-spec: user@example.org
        SingleParseResult::Valid(Mailbox {
            name: entry.to_string(),
            email: Some(entry.to_string()),
        })
    } else {
        // Name-only (e.g., "Debian QA Group")
        let name = entry.trim();
        if name.is_empty() {
            SingleParseResult::Malformed
        } else {
            SingleParseResult::Valid(Mailbox {
                name: name.to_string(),
                email: None,
            })
        }
    }
}

/// Split a mailbox-list on commas, respecting double-quoted strings.
///
/// Commas inside `"..."` are NOT treated as separators.
fn split_mailbox_list(input: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let bytes = input.as_bytes();

    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b',' if !in_quotes => {
                entries.push(&input[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }

    // Last segment
    if start <= input.len() {
        entries.push(&input[start..]);
    }

    entries
}

/// Strip surrounding double quotes from a display name.
fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Heuristic: does this string look like a bare email address?
fn looks_like_email(s: &str) -> bool {
    let s = s.trim();
    // Must contain exactly one `@`, no spaces, and something on both sides
    if s.contains(' ') {
        return false;
    }
    let parts: Vec<&str> = s.split('@').collect();
    parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() && parts[1].contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Issue #5 exact input ---

    #[test]
    fn test_multi_maintainer_comma_separated() {
        let result = parse_mailbox_list(
            "Steve Langasek <vorlon@debian.org>, Michael Vogt <michael.vogt@ubuntu.com>",
        );
        assert_eq!(result.mailboxes.len(), 2);
        assert_eq!(result.malformed_count, 0);
        assert_eq!(result.mailboxes[0].name, "Steve Langasek");
        assert_eq!(
            result.mailboxes[0].email.as_deref(),
            Some("vorlon@debian.org")
        );
        assert_eq!(result.mailboxes[1].name, "Michael Vogt");
        assert_eq!(
            result.mailboxes[1].email.as_deref(),
            Some("michael.vogt@ubuntu.com")
        );
    }

    // --- Quoted display names with internal commas ---

    #[test]
    fn test_quoted_comma_in_name() {
        let result = parse_mailbox_list("\"Doe, Jane\" <jane@example.org>");
        assert_eq!(result.mailboxes.len(), 1);
        assert_eq!(result.malformed_count, 0);
        assert_eq!(result.mailboxes[0].name, "Doe, Jane");
        assert_eq!(
            result.mailboxes[0].email.as_deref(),
            Some("jane@example.org")
        );
    }

    #[test]
    fn test_quoted_comma_mixed_with_multi() {
        let result =
            parse_mailbox_list("\"Doe, Jane\" <jane@example.org>, John Smith <john@example.org>");
        assert_eq!(result.mailboxes.len(), 2);
        assert_eq!(result.malformed_count, 0);
        assert_eq!(result.mailboxes[0].name, "Doe, Jane");
        assert_eq!(result.mailboxes[1].name, "John Smith");
    }

    // --- Bare email ---

    #[test]
    fn test_bare_email() {
        let result = parse_mailbox_list("user@example.org");
        assert_eq!(result.mailboxes.len(), 1);
        assert_eq!(result.malformed_count, 0);
        assert_eq!(result.mailboxes[0].name, "user@example.org");
        assert_eq!(
            result.mailboxes[0].email.as_deref(),
            Some("user@example.org")
        );
    }

    // --- Name-only ---

    #[test]
    fn test_name_only() {
        let result = parse_mailbox_list("Debian QA Group");
        assert_eq!(result.mailboxes.len(), 1);
        assert_eq!(result.malformed_count, 0);
        assert_eq!(result.mailboxes[0].name, "Debian QA Group");
        assert!(result.mailboxes[0].email.is_none());
    }

    // --- Malformed entries ---

    #[test]
    fn test_empty_string() {
        let result = parse_mailbox_list("");
        assert_eq!(result.mailboxes.len(), 0);
        assert_eq!(result.malformed_count, 1);
    }

    #[test]
    fn test_unmatched_angle_bracket() {
        let result = parse_mailbox_list("Name <email");
        assert_eq!(result.mailboxes.len(), 0);
        assert!(result.malformed_count > 0);
    }

    #[test]
    fn test_empty_angle_brackets() {
        let result = parse_mailbox_list("Name <>");
        assert_eq!(result.mailboxes.len(), 0);
        assert!(result.malformed_count > 0);
    }

    #[test]
    fn test_stray_angle_brackets() {
        let result = parse_mailbox_list(">>>");
        assert_eq!(result.mailboxes.len(), 0);
        assert!(result.malformed_count > 0);
    }

    // --- Single standard entry ---

    #[test]
    fn test_single_standard() {
        let result = parse_mailbox_list("GNU Libc Maintainers <debian-glibc@lists.debian.org>");
        assert_eq!(result.mailboxes.len(), 1);
        assert_eq!(result.malformed_count, 0);
        assert_eq!(result.mailboxes[0].name, "GNU Libc Maintainers");
        assert_eq!(
            result.mailboxes[0].email.as_deref(),
            Some("debian-glibc@lists.debian.org")
        );
    }

    // --- IRI safety ---

    #[test]
    fn test_is_email_iri_safe() {
        assert!(is_email_iri_safe("user@example.org"));
        assert!(is_email_iri_safe("debian-glibc@lists.debian.org"));
        assert!(!is_email_iri_safe("user @example.org")); // space
        assert!(!is_email_iri_safe("<user@example.org>")); // angle brackets
        assert!(!is_email_iri_safe("")); // empty
        assert!(!is_email_iri_safe("no-at-sign")); // no @
    }

    // --- Unicode ---

    #[test]
    fn test_unicode_name() {
        let result = parse_mailbox_list("Jos\u{00e9} Garc\u{00ed}a <jose@example.org>");
        assert_eq!(result.mailboxes.len(), 1);
        assert_eq!(result.malformed_count, 0);
        assert_eq!(result.mailboxes[0].name, "Jos\u{00e9} Garc\u{00ed}a");
        assert_eq!(
            result.mailboxes[0].email.as_deref(),
            Some("jose@example.org")
        );
    }

    // --- Edge cases ---

    #[test]
    fn test_whitespace_only() {
        let result = parse_mailbox_list("   ");
        assert_eq!(result.mailboxes.len(), 0);
        assert_eq!(result.malformed_count, 1);
    }

    #[test]
    fn test_email_only_in_brackets() {
        let result = parse_mailbox_list("<user@example.org>");
        assert_eq!(result.mailboxes.len(), 1);
        assert_eq!(result.mailboxes[0].name, "user@example.org");
        assert_eq!(
            result.mailboxes[0].email.as_deref(),
            Some("user@example.org")
        );
    }

    #[test]
    fn test_three_maintainers() {
        let result = parse_mailbox_list("A <a@x.org>, B <b@x.org>, C <c@x.org>");
        assert_eq!(result.mailboxes.len(), 3);
        assert_eq!(result.malformed_count, 0);
    }

    #[test]
    fn test_mixed_valid_and_malformed() {
        let result = parse_mailbox_list("Good Person <good@x.org>, Bad <, Also Good <also@x.org>");
        assert_eq!(result.mailboxes.len(), 2);
        assert_eq!(result.malformed_count, 1);
    }

    // --- IRI safety: additional rejections ---

    #[test]
    fn test_is_email_iri_safe_rejects_double_quotes() {
        assert!(!is_email_iri_safe("\"user\"@example.org"));
    }

    #[test]
    fn test_is_email_iri_safe_rejects_tab() {
        assert!(!is_email_iri_safe("user\t@example.org"));
    }

    #[test]
    fn test_is_email_iri_safe_rejects_control_chars() {
        assert!(!is_email_iri_safe("user\x01@example.org"));
        assert!(!is_email_iri_safe("user\x00@example.org"));
    }

    #[test]
    fn test_is_email_iri_safe_rejects_backslash() {
        assert!(!is_email_iri_safe("user\\name@example.org"));
    }

    // --- Cross-caller consistency: name-only URI identity ---

    #[test]
    fn test_name_only_uri_consistent_across_callers() {
        // Verifies that the parse_mailbox_list -> maintainer_name_uri path
        // (used by debian.rs and collect_sources.rs direct emitters) produces
        // the same URI that the normalize pipeline would generate.
        //
        // The RDF emitter path (emit/rdf.rs) is tested separately in
        // emit::rdf::tests::test_emit_maintainer_name_only_uses_name_uri
        // which verifies the actual emitter calls maintainer_name_uri().
        use crate::ir::MaintainerIr;
        use crate::uris::maintainer_name_uri;

        let name = "Debian QA Group";

        // Path 1: parse_mailbox_list -> maintainer_name_uri (debian.rs, collect_sources.rs)
        let parsed = parse_mailbox_list(name);
        assert_eq!(parsed.mailboxes.len(), 1);
        assert!(parsed.mailboxes[0].email.is_none());
        let uri_direct = maintainer_name_uri(&parsed.mailboxes[0].name);

        // Path 2: normalize pipeline builds MaintainerIr with email=None
        // -> emit/rdf.rs uses maintainer_name_uri(&maint.name)
        let maint_ir = MaintainerIr {
            name: name.to_string(),
            email: None,
            role_hint: Some("maintainer".to_string()),
        };
        let uri_from_ir = maintainer_name_uri(&maint_ir.name);

        assert_eq!(
            uri_direct, uri_from_ir,
            "Name-only URIs must be identical across direct and normalize paths"
        );
        assert!(
            uri_direct.contains("/maintainer/name/"),
            "Name-only URIs must use the /maintainer/name/ path"
        );
        assert!(
            !uri_direct.contains("/maintainer/debian"),
            "Name-only URIs must NOT use the old slugified format"
        );
    }

    // --- Percent-encoding in emails ---

    #[test]
    fn test_is_email_iri_safe_rejects_bare_percent() {
        // %zz is not valid percent-encoding
        assert!(!is_email_iri_safe("user%zz@example.org"));
    }

    #[test]
    fn test_is_email_iri_safe_accepts_valid_percent_encoding() {
        // %2B is valid percent-encoding for '+'
        assert!(is_email_iri_safe("user%2B@example.org"));
    }

    #[test]
    fn test_is_email_iri_safe_rejects_percent_at_end() {
        // % at end of string — not followed by 2 hex digits
        assert!(!is_email_iri_safe("user%@example.org"));
    }

    #[test]
    fn test_is_email_iri_safe_rejects_percent_one_hex() {
        // %2 — only one hex digit after %
        assert!(!is_email_iri_safe("user%2@example.org"));
    }
}
