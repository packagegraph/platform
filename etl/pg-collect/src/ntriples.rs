use std::fs::File;
use std::io::{BufWriter, Write, Result};

/// Streaming N-Triples writer.
///
/// Uses BufWriter for I/O buffering. Flush happens automatically on drop.
pub struct NTriplesWriter {
    writer: BufWriter<File>,
}

impl NTriplesWriter {
    /// Create a new N-Triples writer for the given file.
    pub fn new(file: File) -> Self {
        Self {
            writer: BufWriter::new(file),
        }
    }

    /// Write a triple: `<subject> <predicate> <object> .\n`
    ///
    /// Validates that URIs don't contain characters illegal in N-Triples IRIs.
    /// Skips the triple and warns on stderr if any URI is malformed.
    pub fn write_triple(&mut self, subject: &str, predicate: &str, object: &str) -> Result<()> {
        if has_invalid_iri_chars(subject) || has_invalid_iri_chars(predicate) || has_invalid_iri_chars(object) {
            eprintln!("WARNING: skipping triple with invalid URI character: <{}> <{}> <{}>",
                &subject[..subject.len().min(80)],
                &predicate[..predicate.len().min(80)],
                &object[..object.len().min(80)]);
            return Ok(());
        }
        writeln!(self.writer, "<{subject}> <{predicate}> <{object}> .")
    }

    /// Write a literal triple: `<subject> <predicate> "value" .\n`
    ///
    /// Escapes: \\ \" \n \r \t in the literal value.
    pub fn write_literal(&mut self, subject: &str, predicate: &str, value: &str) -> Result<()> {
        let escaped = escape_literal(value);
        writeln!(self.writer, "<{subject}> <{predicate}> \"{escaped}\" .")
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
            "<{subject}> <{predicate}> \"{escaped}\"^^<{datatype}> ."
        )
    }

    /// Write an integer literal with xsd:integer datatype.
    pub fn write_integer(&mut self, subject: &str, predicate: &str, value: i64) -> Result<()> {
        writeln!(
            self.writer,
            "<{subject}> <{predicate}> \"{value}\"^^<http://www.w3.org/2001/XMLSchema#integer> ."
        )
    }

    /// Write a boolean literal with xsd:boolean datatype.
    pub fn write_boolean(&mut self, subject: &str, predicate: &str, value: bool) -> Result<()> {
        let bool_str = if value { "true" } else { "false" };
        writeln!(
            self.writer,
            "<{subject}> <{predicate}> \"{bool_str}\"^^<http://www.w3.org/2001/XMLSchema#boolean> ."
        )
    }

    /// Write a dateTime literal with xsd:dateTime datatype.
    pub fn write_datetime(&mut self, subject: &str, predicate: &str, value: &str) -> Result<()> {
        let escaped = escape_literal(value);
        writeln!(
            self.writer,
            "<{subject}> <{predicate}> \"{escaped}\"^^<http://www.w3.org/2001/XMLSchema#dateTime> ."
        )
    }

    /// Write a blank node triple: `<subject> <predicate> _:bnode .\n`
    pub fn write_bnode_object(&mut self, subject: &str, predicate: &str, bnode: &str) -> Result<()> {
        writeln!(self.writer, "<{subject}> <{predicate}> _:{bnode} .")
    }

    /// Write a triple with blank node subject: `_:bnode <predicate> <object> .\n`
    pub fn write_bnode_subject(&mut self, bnode: &str, predicate: &str, object: &str) -> Result<()> {
        writeln!(self.writer, "_:{bnode} <{predicate}> <{object}> .")
    }

    /// Write a literal with blank node subject: `_:bnode <predicate> "literal" .\n`
    pub fn write_bnode_literal(&mut self, bnode: &str, predicate: &str, value: &str) -> Result<()> {
        let escaped = escape_literal(value);
        writeln!(self.writer, "_:{bnode} <{predicate}> \"{escaped}\" .")
    }

    /// Flush the buffer (called automatically on drop).
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()
    }
}

/// Check if a URI string contains characters that are illegal in N-Triples IRIs.
/// N-Triples IRIs must not contain: < > " { } | ^ ` \ or unescaped whitespace.
/// Also rejects bare `%` not followed by two hex digits (invalid percent-encoding).
fn has_invalid_iri_chars(uri: &str) -> bool {
    let bytes = uri.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'<' | b'>' | b'"' | b'{' | b'}' | b'|' | b'^' | b'`' | b'\\' | b' ' | b'\t' | b'\n' | b'\r' => {
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
fn escape_literal(s: &str) -> String {
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
        writer.write_bnode_subject("dep1", "http://www.w3.org/1999/02/22-rdf-syntax-ns#type", "https://example.org/Dependency")?;
        writer.flush()?;

        let mut content = String::new();
        temp_file.reopen()?.read_to_string(&mut content)?;
        assert!(content.contains("_:dep1"));

        Ok(())
    }

    #[test]
    fn test_has_invalid_iri_chars() {
        assert!(!has_invalid_iri_chars("https://example.org/foo/bar"));
        assert!(!has_invalid_iri_chars("https://example.org/foo%2Fbar"));  // valid percent-encoding
        assert!(!has_invalid_iri_chars("https://example.org/foo%25bar"));  // encoded %
        assert!(has_invalid_iri_chars("https://example.org/foo bar"));     // space
        assert!(has_invalid_iri_chars("https://example.org/foo%xyz"));     // bare % (not followed by hex)
        assert!(has_invalid_iri_chars("https://example.org/foo%GGbar"));   // % followed by non-hex
        assert!(has_invalid_iri_chars("https://example.org/<foo>"));       // angle brackets
        assert!(has_invalid_iri_chars("https://example.org/foo\"bar"));    // quote
        assert!(has_invalid_iri_chars("https://example.org/foo\\bar"));    // backslash
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

        assert_eq!(content.lines().count(), 1, "Only valid triple should be written");
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
}
