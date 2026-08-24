use crate::uris::{PKG, SEC, VCS};
use std::fs::File;
use std::io::{BufWriter, Result, Write};

/// Look up the inverse predicate for a given forward predicate.
///
/// Returns Some(inverse_uri) if the predicate has a mapped inverse, None otherwise.
/// Matches on the predicate suffix (after #) for efficiency.
fn lookup_inverse(predicate: &str) -> Option<String> {
    let suffix = predicate.rsplit('#').next()?;
    match suffix {
        "hasVersion" => Some(format!("{PKG}versionOf")),
        "directlyDependsOn" => Some(format!("{PKG}isDirectDependencyOf")),
        "isVersionOf" => Some(format!("{PKG}hasPackage")),
        "maintainedBy" => Some(format!("{PKG}maintains")),
        "builtFromSource" => Some(format!("{PKG}producedBinary")),
        "upstreamRepository" => Some(format!("{VCS}hasPackage")),
        "affectsVersion" => Some(format!("{SEC}vulnerableIn")),
        "partOfDistribution" => Some(format!("{PKG}hasRelease")),
        _ => None,
    }
}

/// Streaming N-Triples / N-Quads writer.
///
/// Uses BufWriter for I/O buffering. Flush happens automatically on drop.
/// When constructed with `with_graph`, emits N-Quads (each line includes a graph URI).
/// When constructed with `new`, emits standard N-Triples (no graph term).
pub struct NTriplesWriter {
    writer: BufWriter<File>,
    /// Count of triples skipped due to invalid IRI characters.
    pub skipped_invalid_iri: usize,
    /// Pre-formatted line suffix: " ." for N-Triples, " <graph_uri> ." for N-Quads.
    line_suffix: String,
}

impl NTriplesWriter {
    /// Create a new N-Triples writer for the given file.
    pub fn new(file: File) -> Self {
        Self {
            writer: BufWriter::new(file),
            skipped_invalid_iri: 0,
            line_suffix: " .".to_string(),
        }
    }

    /// Create a new N-Quads writer that includes the given graph URI on every line.
    pub fn with_graph(file: File, graph_uri: String) -> Self {
        Self {
            writer: BufWriter::new(file),
            skipped_invalid_iri: 0,
            line_suffix: format!(" <{}> .", graph_uri),
        }
    }

    /// Create a writer, choosing N-Quads or N-Triples mode based on the optional graph URI.
    pub fn new_maybe_graph(file: File, graph_uri: Option<&str>) -> Self {
        match graph_uri {
            Some(uri) => Self::with_graph(file, uri.to_string()),
            None => Self::new(file),
        }
    }

    /// Write a triple: `<subject> <predicate> <object> .\n`
    ///
    /// Validates that URIs don't contain characters illegal in N-Triples IRIs.
    /// Skips the triple and warns on stderr if any URI is malformed.
    ///
    /// Auto-emits the inverse triple if the predicate has a known inverse mapping.
    pub fn write_triple(&mut self, subject: &str, predicate: &str, object: &str) -> Result<()> {
        if has_invalid_iri_chars(subject)
            || has_invalid_iri_chars(predicate)
            || has_invalid_iri_chars(object)
        {
            self.skipped_invalid_iri += 1;
            if self.skipped_invalid_iri <= 10 {
                eprintln!(
                    "WARNING: skipping triple with invalid URI character: <{}> <{}> <{}>",
                    &subject[..subject.len().min(80)],
                    &predicate[..predicate.len().min(80)],
                    &object[..object.len().min(80)]
                );
            } else if self.skipped_invalid_iri == 11 {
                eprintln!("WARNING: suppressing further invalid URI warnings (total so far: 11)");
            }
            return Ok(());
        }
        writeln!(
            self.writer,
            "<{subject}> <{predicate}> <{object}>{}",
            self.line_suffix
        )?;

        // Auto-emit inverse triple if predicate has a known inverse
        if let Some(inverse_pred) = lookup_inverse(predicate) {
            writeln!(
                self.writer,
                "<{object}> <{inverse_pred}> <{subject}>{}",
                self.line_suffix
            )?;
        }

        Ok(())
    }

    /// Write a literal triple: `<subject> <predicate> "value" .\n`
    ///
    /// Escapes: \\ \" \n \r \t in the literal value.
    pub fn write_literal(&mut self, subject: &str, predicate: &str, value: &str) -> Result<()> {
        let escaped = escape_literal(value);
        writeln!(
            self.writer,
            "<{subject}> <{predicate}> \"{escaped}\"{}",
            self.line_suffix
        )
    }

    /// Write a typed literal: `<subject> <predicate> "value"^^<datatype> .\n`
    pub fn write_typed_literal(
        &mut self,
        subject: &str,
        predicate: &str,
        value: &str,
        datatype: &str,
    ) -> Result<()> {
        let escaped = escape_literal(value);
        writeln!(
            self.writer,
            "<{subject}> <{predicate}> \"{escaped}\"^^<{datatype}>{}",
            self.line_suffix
        )
    }

    /// Write an integer literal with xsd:integer datatype.
    pub fn write_integer(&mut self, subject: &str, predicate: &str, value: i64) -> Result<()> {
        writeln!(
            self.writer,
            "<{subject}> <{predicate}> \"{value}\"^^<http://www.w3.org/2001/XMLSchema#integer>{}",
            self.line_suffix
        )
    }

    /// Write a boolean literal with xsd:boolean datatype.
    pub fn write_boolean(&mut self, subject: &str, predicate: &str, value: bool) -> Result<()> {
        let bool_str = if value { "true" } else { "false" };
        writeln!(
            self.writer,
            "<{subject}> <{predicate}> \"{bool_str}\"^^<http://www.w3.org/2001/XMLSchema#boolean>{}", self.line_suffix
        )
    }

    /// Write a dateTime literal with xsd:dateTime datatype.
    pub fn write_datetime(&mut self, subject: &str, predicate: &str, value: &str) -> Result<()> {
        let escaped = escape_literal(value);
        writeln!(
            self.writer,
            "<{subject}> <{predicate}> \"{escaped}\"^^<http://www.w3.org/2001/XMLSchema#dateTime>{}", self.line_suffix
        )
    }

    /// Write a date literal with xsd:date datatype (format: YYYY-MM-DD).
    pub fn write_date(&mut self, subject: &str, predicate: &str, value: &str) -> Result<()> {
        let escaped = escape_literal(value);
        writeln!(
            self.writer,
            "<{subject}> <{predicate}> \"{escaped}\"^^<http://www.w3.org/2001/XMLSchema#date>{}",
            self.line_suffix
        )
    }

    /// Write a blank node triple: `<subject> <predicate> _:bnode .\n`
    pub fn write_bnode_object(
        &mut self,
        subject: &str,
        predicate: &str,
        bnode: &str,
    ) -> Result<()> {
        writeln!(
            self.writer,
            "<{subject}> <{predicate}> _:{bnode}{}",
            self.line_suffix
        )
    }

    /// Write a triple with blank node subject: `_:bnode <predicate> <object> .\n`
    pub fn write_bnode_subject(
        &mut self,
        bnode: &str,
        predicate: &str,
        object: &str,
    ) -> Result<()> {
        writeln!(
            self.writer,
            "_:{bnode} <{predicate}> <{object}>{}",
            self.line_suffix
        )
    }

    /// Write a triple with both blank node subject and blank node object: `_:s <predicate> _:o .\n`
    pub fn write_bnode_to_bnode(
        &mut self,
        subject_bnode: &str,
        predicate: &str,
        object_bnode: &str,
    ) -> Result<()> {
        writeln!(
            self.writer,
            "_:{subject_bnode} <{predicate}> _:{object_bnode}{}",
            self.line_suffix
        )
    }

    /// Write a literal with blank node subject: `_:bnode <predicate> "literal" .\n`
    pub fn write_bnode_literal(&mut self, bnode: &str, predicate: &str, value: &str) -> Result<()> {
        let escaped = escape_literal(value);
        writeln!(
            self.writer,
            "_:{bnode} <{predicate}> \"{escaped}\"{}",
            self.line_suffix
        )
    }

    /// Write a raw N-Triple line directly (for pre-formatted triples from format functions).
    /// In N-Quads mode, replaces the trailing " ." with " <graph> .".
    pub fn write_raw_line(&mut self, line: &str) -> Result<()> {
        if let Some(stripped) = line.strip_suffix(" .") {
            writeln!(self.writer, "{}{}", stripped, self.line_suffix)
        } else {
            writeln!(self.writer, "{}", line)
        }
    }

    /// Flush the buffer (called automatically on drop).
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()
    }
}

/// Check if a URI string contains characters that are illegal in N-Triples IRIs.
/// N-Triples IRIs must not contain: < > " { } | ^ ` \ or unescaped whitespace.
/// Also rejects bare `%` not followed by two hex digits (invalid percent-encoding).
pub(crate) fn has_invalid_iri_chars(uri: &str) -> bool {
    let bytes = uri.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'<' | b'>' | b'"' | b'{' | b'}' | b'|' | b'^' | b'`' | b'\\' | b' ' | b'\t'
            | b'\n' | b'\r' => {
                return true;
            }
            b'%' => {
                // Must be followed by exactly two hex digits
                if i + 2 >= bytes.len()
                    || !bytes[i + 1].is_ascii_hexdigit()
                    || !bytes[i + 2].is_ascii_hexdigit()
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Escape a string literal for N-Triples.
///
/// W3C N-Triples grammar STRING_LITERAL_QUOTE escaping:
/// - \\ (backslash)
/// - \" (double quote)
/// - \n (newline)
/// - \r (carriage return)
/// - \t (tab)
///
/// Non-ASCII characters are passed through as UTF-8 (N-Triples allows this).
pub(crate) fn escape_literal(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            _ => result.push(ch),
        }
    }
    result
}

/// Percent-encode a PURL component per purl-spec rules.
/// Encodes everything except unreserved characters: `[-._~a-zA-Z0-9]`
fn purl_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

/// Format a Package URL (PURL) string according to the purl-spec.
///
/// Percent-encodes name, version, namespace, and qualifier values per spec.
/// Qualifiers are sorted by key for canonical output.
///
/// See: https://github.com/package-url/purl-spec
///
/// Example: format_purl("rpm", Some("fedora"), "openssl", Some("3.2.2-6.fc43"), &[("arch", "x86_64")])
///          → "pkg:rpm/fedora/openssl@3.2.2-6.fc43?arch=x86_64"
pub fn format_purl(
    purl_type: &str,
    namespace: Option<&str>,
    name: &str,
    version: Option<&str>,
    qualifiers: &[(&str, &str)],
) -> String {
    let mut purl = format!("pkg:{}", purl_type);
    if let Some(ns) = namespace {
        purl.push('/');
        purl.push_str(&purl_encode(ns));
    }
    purl.push('/');
    purl.push_str(&purl_encode(name));
    if let Some(ver) = version {
        purl.push('@');
        purl.push_str(&purl_encode(ver));
    }
    if !qualifiers.is_empty() {
        let mut sorted: Vec<_> = qualifiers.to_vec();
        sorted.sort_by_key(|(k, _)| *k);
        purl.push('?');
        for (i, (k, v)) in sorted.iter().enumerate() {
            if i > 0 {
                purl.push('&');
            }
            purl.push_str(k);
            purl.push('=');
            purl.push_str(&purl_encode(v));
        }
    }
    purl
}

/// Generate a deterministic blank node ID from content hash.
///
/// Uses format `dep_{hash}` where hash is based on the content.
/// This avoids blank node conflicts when files are loaded separately.
pub fn bnode_id(prefix: &str, content: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    let hash = hasher.finish();

    format!("{prefix}_{hash:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_escape_literal_basic() {
        assert_eq!(escape_literal("hello"), "hello");
        assert_eq!(escape_literal("hello world"), "hello world");
    }

    #[test]
    fn test_escape_literal_special_chars() {
        assert_eq!(escape_literal("hello\\world"), "hello\\\\world");
        assert_eq!(escape_literal("hello\"world"), "hello\\\"world");
        assert_eq!(escape_literal("hello\nworld"), "hello\\nworld");
        assert_eq!(escape_literal("hello\rworld"), "hello\\rworld");
        assert_eq!(escape_literal("hello\tworld"), "hello\\tworld");
    }

    #[test]
    fn test_escape_literal_multi_line() {
        let input = "First line\nSecond line\nThird line";
        let expected = "First line\\nSecond line\\nThird line";
        assert_eq!(escape_literal(input), expected);
    }

    #[test]
    fn test_write_triple() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp_file.reopen()?);

        writer.write_triple(
            "https://example.org/subject",
            "https://example.org/predicate",
            "https://example.org/object",
        )?;
        writer.flush()?;

        let mut content = String::new();
        temp_file.reopen()?.read_to_string(&mut content)?;
        assert_eq!(
            content,
            "<https://example.org/subject> <https://example.org/predicate> <https://example.org/object> .\n"
        );

        Ok(())
    }

    #[test]
    fn test_write_literal() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp_file.reopen()?);

        writer.write_literal(
            "https://example.org/subject",
            "https://example.org/predicate",
            "hello world",
        )?;
        writer.flush()?;

        let mut content = String::new();
        temp_file.reopen()?.read_to_string(&mut content)?;
        assert_eq!(
            content,
            "<https://example.org/subject> <https://example.org/predicate> \"hello world\" .\n"
        );

        Ok(())
    }

    #[test]
    fn test_write_literal_with_escapes() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp_file.reopen()?);

        writer.write_literal(
            "https://example.org/pkg",
            "https://example.org/desc",
            "Line 1\nLine 2\tTabbed",
        )?;
        writer.flush()?;

        let mut content = String::new();
        temp_file.reopen()?.read_to_string(&mut content)?;
        assert_eq!(
            content,
            "<https://example.org/pkg> <https://example.org/desc> \"Line 1\\nLine 2\\tTabbed\" .\n"
        );

        Ok(())
    }

    #[test]
    fn test_write_typed_literal() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp_file.reopen()?);

        writer.write_typed_literal(
            "https://example.org/subject",
            "https://example.org/predicate",
            "123",
            "http://www.w3.org/2001/XMLSchema#integer",
        )?;
        writer.flush()?;

        let mut content = String::new();
        temp_file.reopen()?.read_to_string(&mut content)?;
        assert_eq!(
            content,
            "<https://example.org/subject> <https://example.org/predicate> \"123\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n"
        );

        Ok(())
    }

    #[test]
    fn test_write_bnode() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp_file.reopen()?);

        writer.write_bnode_object(
            "https://example.org/pkg",
            "https://example.org/hasDep",
            "dep1",
        )?;
        writer.write_bnode_subject(
            "dep1",
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "https://example.org/Dependency",
        )?;
        writer.flush()?;

        let mut content = String::new();
        temp_file.reopen()?.read_to_string(&mut content)?;
        assert!(content.contains("_:dep1"));

        Ok(())
    }

    #[test]
    fn test_has_invalid_iri_chars() {
        assert!(!has_invalid_iri_chars("https://example.org/foo/bar"));
        assert!(!has_invalid_iri_chars("https://example.org/foo%2Fbar")); // valid percent-encoding
        assert!(!has_invalid_iri_chars("https://example.org/foo%25bar")); // encoded %
        assert!(has_invalid_iri_chars("https://example.org/foo bar")); // space
        assert!(has_invalid_iri_chars("https://example.org/foo%xyz")); // bare % (not followed by hex)
        assert!(has_invalid_iri_chars("https://example.org/foo%GGbar")); // % followed by non-hex
        assert!(has_invalid_iri_chars("https://example.org/<foo>")); // angle brackets
        assert!(has_invalid_iri_chars("https://example.org/foo\"bar")); // quote
        assert!(has_invalid_iri_chars("https://example.org/foo\\bar")); // backslash
    }

    #[test]
    fn test_write_triple_skips_invalid_uris() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp_file.reopen()?);

        // Valid triple — should be written
        writer.write_triple(
            "https://example.org/s",
            "https://example.org/p",
            "https://example.org/o",
        )?;

        // Invalid triple — object has bare % — should be skipped
        writer.write_triple(
            "https://example.org/s",
            "https://example.org/p",
            "https://example.org/bad%object",
        )?;

        writer.flush()?;

        let mut content = String::new();
        temp_file.reopen()?.read_to_string(&mut content)?;

        assert_eq!(
            content.lines().count(),
            1,
            "Only valid triple should be written"
        );
        assert!(content.contains("example.org/o"));
        assert!(!content.contains("bad%object"));

        Ok(())
    }

    #[test]
    fn test_bnode_id_deterministic() {
        let id1 = bnode_id("dep", "libc6");
        let id2 = bnode_id("dep", "libc6");
        assert_eq!(id1, id2);

        let id3 = bnode_id("dep", "gcc");
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_write_triple_emits_inverse_for_hasVersion() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp_file.reopen()?);

        writer.write_triple(
            "https://packagegraph.github.io/d/pkg/debian/trixie/amd64/libc6/2.36-1",
            "https://purl.org/packagegraph/ontology/core#hasVersion",
            "https://packagegraph.github.io/d/ver/debian/trixie/libc6/2.36-1",
        )?;
        writer.flush()?;

        let mut content = String::new();
        temp_file.reopen()?.read_to_string(&mut content)?;

        // Should emit both forward and inverse triples
        assert_eq!(content.lines().count(), 2, "Should emit forward + inverse");
        assert!(content.contains("hasVersion"));
        assert!(content.contains("versionOf"));

        // Verify inverse has swapped subject/object
        assert!(content.contains("<https://packagegraph.github.io/d/ver/debian/trixie/libc6/2.36-1> <https://purl.org/packagegraph/ontology/core#versionOf> <https://packagegraph.github.io/d/pkg/debian/trixie/amd64/libc6/2.36-1>"));

        Ok(())
    }

    #[test]
    fn test_write_triple_no_inverse_for_unmapped_predicate() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp_file.reopen()?);

        writer.write_triple(
            "https://example.org/subject",
            "https://example.org/someOtherPredicate",
            "https://example.org/object",
        )?;
        writer.flush()?;

        let mut content = String::new();
        temp_file.reopen()?.read_to_string(&mut content)?;

        // Should emit only the forward triple (no inverse for unmapped predicate)
        assert_eq!(
            content.lines().count(),
            1,
            "Should emit only forward triple"
        );

        Ok(())
    }

    #[test]
    fn test_all_inverse_predicates_covered() {
        // Verify all 8 forward predicates have inverses defined
        let test_cases = vec![
            ("hasVersion", "versionOf"),
            ("directlyDependsOn", "isDirectDependencyOf"),
            ("isVersionOf", "hasPackage"),
            ("maintainedBy", "maintains"),
            ("builtFromSource", "producedBinary"),
            ("upstreamRepository", "hasPackage"), // vcs:hasPackage
            ("affectsVersion", "vulnerableIn"),
            ("partOfDistribution", "hasRelease"),
        ];

        for (forward, inverse) in test_cases {
            let full_forward = format!("https://purl.org/packagegraph/ontology/core#{}", forward);
            let result = lookup_inverse(&full_forward);
            assert!(result.is_some(), "Missing inverse for {}", forward);
            assert!(
                result.unwrap().contains(inverse),
                "Wrong inverse for {}",
                forward
            );
        }
    }

    #[test]
    fn test_write_boolean() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp_file.reopen()?);

        writer.write_boolean(
            "https://example.org/env",
            "https://example.org/isEphemeral",
            true,
        )?;
        writer.flush()?;

        let mut content = String::new();
        temp_file.reopen()?.read_to_string(&mut content)?;
        assert_eq!(
            content,
            "<https://example.org/env> <https://example.org/isEphemeral> \"true\"^^<http://www.w3.org/2001/XMLSchema#boolean> .\n"
        );

        Ok(())
    }

    #[test]
    fn test_write_datetime() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp_file.reopen()?);

        writer.write_datetime(
            "https://example.org/att",
            "https://example.org/timestamp",
            "2026-04-13T10:00:00Z",
        )?;
        writer.flush()?;

        let mut content = String::new();
        temp_file.reopen()?.read_to_string(&mut content)?;
        assert_eq!(
            content,
            "<https://example.org/att> <https://example.org/timestamp> \"2026-04-13T10:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime> .\n"
        );

        Ok(())
    }

    #[test]
    fn test_write_date() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp_file.reopen()?);

        writer.write_date(
            "https://example.org/pkg",
            "https://example.org/releaseDate",
            "2024-04-18",
        )?;
        writer.flush()?;

        let mut content = String::new();
        temp_file.reopen()?.read_to_string(&mut content)?;
        assert_eq!(
            content,
            "<https://example.org/pkg> <https://example.org/releaseDate> \"2024-04-18\"^^<http://www.w3.org/2001/XMLSchema#date> .\n"
        );

        Ok(())
    }

    #[test]
    fn test_bnode_to_bnode_produces_correct_syntax() -> Result<()> {
        let temp = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp.reopen()?);

        writer.write_bnode_to_bnode("dep1", "http://example.org/hasConstraint", "constraint1")?;
        writer.flush()?;

        let mut content = String::new();
        temp.reopen()?.read_to_string(&mut content)?;

        assert!(
            content.contains("_:dep1"),
            "Subject should be blank node _:dep1, got: {}",
            content
        );
        assert!(
            content.contains("_:constraint1"),
            "Object should be blank node _:constraint1, got: {}",
            content
        );
        assert!(
            !content.contains("<dep1>"),
            "Subject should NOT be a URI <dep1>"
        );
        assert!(
            !content.contains("<constraint1>"),
            "Object should NOT be a URI <constraint1>"
        );

        Ok(())
    }

    #[test]
    fn test_bnode_object_does_not_produce_bnode_subject() -> Result<()> {
        let temp = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(temp.reopen()?);

        writer.write_bnode_object(
            "http://example.org/pkg1",
            "http://example.org/hasDep",
            "dep1",
        )?;
        writer.flush()?;

        let mut content = String::new();
        temp.reopen()?.read_to_string(&mut content)?;

        assert!(
            content.contains("<http://example.org/pkg1>"),
            "Subject should be a URI"
        );
        assert!(content.contains("_:dep1"), "Object should be blank node");

        Ok(())
    }

    #[test]
    fn test_nquads_write_triple() -> Result<()> {
        let tmp = NamedTempFile::new()?;
        let mut writer =
            NTriplesWriter::with_graph(tmp.reopen()?, "https://example.org/graph/test".to_string());
        writer.write_triple(
            "https://example.org/s",
            "https://example.org/p",
            "https://example.org/o",
        )?;
        writer.flush()?;

        let mut content = String::new();
        tmp.reopen()?.read_to_string(&mut content)?;
        assert_eq!(
            content,
            "<https://example.org/s> <https://example.org/p> <https://example.org/o> <https://example.org/graph/test> .\n"
        );

        Ok(())
    }

    #[test]
    fn test_nquads_write_literal() -> Result<()> {
        let tmp = NamedTempFile::new()?;
        let mut writer =
            NTriplesWriter::with_graph(tmp.reopen()?, "https://example.org/graph/test".to_string());
        writer.write_literal("https://example.org/s", "https://example.org/p", "hello")?;
        writer.flush()?;

        let mut content = String::new();
        tmp.reopen()?.read_to_string(&mut content)?;
        assert_eq!(
            content,
            "<https://example.org/s> <https://example.org/p> \"hello\" <https://example.org/graph/test> .\n"
        );

        Ok(())
    }

    #[test]
    fn test_ntriples_mode_unchanged() -> Result<()> {
        let tmp = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(tmp.reopen()?);
        writer.write_triple(
            "https://example.org/s",
            "https://example.org/p",
            "https://example.org/o",
        )?;
        writer.flush()?;

        let mut content = String::new();
        tmp.reopen()?.read_to_string(&mut content)?;
        // Standard N-Triples: no graph term
        assert_eq!(
            content,
            "<https://example.org/s> <https://example.org/p> <https://example.org/o> .\n"
        );
        assert!(!content.contains("graph"));

        Ok(())
    }

    #[test]
    fn test_nquads_bnode() -> Result<()> {
        let tmp = NamedTempFile::new()?;
        let mut writer =
            NTriplesWriter::with_graph(tmp.reopen()?, "https://example.org/graph/test".to_string());
        writer.write_bnode_object("https://example.org/s", "https://example.org/p", "b1")?;
        writer.write_bnode_subject("b1", "https://example.org/q", "https://example.org/o")?;
        writer.write_bnode_to_bnode("b1", "https://example.org/r", "b2")?;
        writer.write_bnode_literal("b1", "https://example.org/name", "test")?;
        writer.flush()?;

        let mut content = String::new();
        tmp.reopen()?.read_to_string(&mut content)?;
        // All lines should contain the graph URI
        for line in content.lines() {
            assert!(
                line.contains("<https://example.org/graph/test> ."),
                "Line missing graph URI: {}",
                line
            );
        }
        assert_eq!(content.lines().count(), 4);

        Ok(())
    }

    #[test]
    fn test_nquads_inverse_triple_includes_graph() -> Result<()> {
        let tmp = NamedTempFile::new()?;
        let mut writer =
            NTriplesWriter::with_graph(tmp.reopen()?, "https://example.org/graph/test".to_string());
        writer.write_triple(
            "https://packagegraph.github.io/d/pkg/debian/trixie/amd64/libc6/2.36-1",
            "https://purl.org/packagegraph/ontology/core#hasVersion",
            "https://packagegraph.github.io/d/ver/debian/trixie/libc6/2.36-1",
        )?;
        writer.flush()?;

        let mut content = String::new();
        tmp.reopen()?.read_to_string(&mut content)?;
        // Both forward and inverse triples should include the graph URI
        assert_eq!(content.lines().count(), 2, "Should emit forward + inverse");
        for line in content.lines() {
            assert!(
                line.contains("<https://example.org/graph/test> ."),
                "Line missing graph URI: {}",
                line
            );
        }
        assert!(content.contains("hasVersion"));
        assert!(content.contains("versionOf"));

        Ok(())
    }

    #[test]
    fn test_new_maybe_graph_with_some() -> Result<()> {
        let tmp = NamedTempFile::new()?;
        let mut writer =
            NTriplesWriter::new_maybe_graph(tmp.reopen()?, Some("https://example.org/g"));
        writer.write_triple(
            "https://example.org/s",
            "https://example.org/p",
            "https://example.org/o",
        )?;
        writer.flush()?;

        let mut content = String::new();
        tmp.reopen()?.read_to_string(&mut content)?;
        assert!(content.contains("<https://example.org/g> ."));

        Ok(())
    }

    #[test]
    fn test_new_maybe_graph_with_none() -> Result<()> {
        let tmp = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new_maybe_graph(tmp.reopen()?, None);
        writer.write_triple(
            "https://example.org/s",
            "https://example.org/p",
            "https://example.org/o",
        )?;
        writer.flush()?;

        let mut content = String::new();
        tmp.reopen()?.read_to_string(&mut content)?;
        assert_eq!(
            content,
            "<https://example.org/s> <https://example.org/p> <https://example.org/o> .\n"
        );

        Ok(())
    }

    #[test]
    fn test_write_raw_line_ntriples() -> Result<()> {
        let tmp = NamedTempFile::new()?;
        let mut writer = NTriplesWriter::new(tmp.reopen()?);
        writer.write_raw_line("<https://ex.org/s> <https://ex.org/p> \"val\" .")?;
        writer.flush()?;

        let mut content = String::new();
        tmp.reopen()?.read_to_string(&mut content)?;
        assert_eq!(content, "<https://ex.org/s> <https://ex.org/p> \"val\" .\n");
        Ok(())
    }

    #[test]
    fn test_write_raw_line_nquads() -> Result<()> {
        let tmp = NamedTempFile::new()?;
        let mut writer =
            NTriplesWriter::with_graph(tmp.reopen()?, "https://example.org/graph".to_string());
        writer.write_raw_line("<https://ex.org/s> <https://ex.org/p> \"val\" .")?;
        writer.flush()?;

        let mut content = String::new();
        tmp.reopen()?.read_to_string(&mut content)?;
        assert_eq!(
            content,
            "<https://ex.org/s> <https://ex.org/p> \"val\" <https://example.org/graph> .\n"
        );
        Ok(())
    }
}
