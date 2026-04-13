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
    pub fn write_triple(&mut self, subject: &str, predicate: &str, object: &str) -> Result<()> {
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
    fn test_bnode_id_deterministic() {
        let id1 = bnode_id("dep", "libc6");
        let id2 = bnode_id("dep", "libc6");
        assert_eq!(id1, id2);

        let id3 = bnode_id("dep", "gcc");
        assert_ne!(id1, id3);
    }
}
