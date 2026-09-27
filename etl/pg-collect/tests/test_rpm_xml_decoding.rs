//! The repodata parser must hand back the values the XML encodes, not the
//! bytes between the quotes.
//!
//! RPM's rich dependency syntax uses `<` and `>`, which XML has to escape. The
//! parser read `attr.value` raw, so a boolean requirement arrived as
//! `((adobe-afdko &gt;= 4.0.1) with (adobe-afdko &lt; 5~~))` and went into the
//! graph that way. The served corpus carries 12,857 identity names containing
//! a literal `&gt;` against four containing a real `>`.
//!
//! The other half is error handling: attributes were read through
//! `.flatten()`, so a malformed one was silently skipped and the record was
//! emitted as though the source had never said it.

use pg_collect::rpm::stream_primary_packages;

fn primary(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata xmlns="http://linux.duke.edu/metadata/common" xmlns:rpm="http://linux.duke.edu/metadata/rpm" packages="1">
{body}
</metadata>"#
    )
}

fn parse(xml: &str) -> std::io::Result<Vec<pg_collect::rpm::RpmPackageData>> {
    let mut packages = Vec::new();
    stream_primary_packages(std::io::BufReader::new(xml.as_bytes()), |pkg| {
        packages.push(pkg);
        Ok(())
    })?;
    Ok(packages)
}

#[test]
fn a_rich_dependency_keeps_the_operators_the_source_wrote() {
    let xml = primary(
        r#"<package type="rpm">
  <name>adobe-afdko-fonts</name>
  <version epoch="0" ver="1.0" rel="1.el9"/>
  <format>
    <rpm:requires>
      <rpm:entry name="((adobe-afdko &gt;= 4.0.1) with (adobe-afdko &lt; 5~~))"/>
    </rpm:requires>
  </format>
</package>"#,
    );

    let packages = parse(&xml).expect("parse failed");
    let dep = &packages[0].deps[0];

    assert_eq!(
        dep.name, "((adobe-afdko >= 4.0.1) with (adobe-afdko < 5~~))",
        "the parser handed back the escaped source text instead of its value"
    );
    assert!(
        !dep.name.contains("&gt;") && !dep.name.contains("&lt;"),
        "an entity reference reached the graph: {}",
        dep.name
    );
}

#[test]
fn ampersands_and_numeric_references_are_decoded_too() {
    let xml = primary(
        r#"<package type="rpm">
  <name>ampersand</name>
  <version epoch="0" ver="1.0" rel="1"/>
  <format>
    <rpm:provides>
      <rpm:entry name="config(a&amp;b)" ver="1&#x2d;0"/>
      <rpm:entry name="tool&#38;lib"/>
    </rpm:provides>
  </format>
</package>"#,
    );

    let packages = parse(&xml).expect("parse failed");
    let names: Vec<&str> = packages[0].deps.iter().map(|d| d.name.as_str()).collect();

    assert!(names.contains(&"config(a&b)"), "named entity: {names:?}");
    assert!(names.contains(&"tool&lib"), "decimal reference: {names:?}");
    assert_eq!(
        packages[0].deps[0].ver.as_deref(),
        Some("1-0"),
        "hexadecimal reference in a version"
    );
}

#[test]
fn a_package_name_element_is_not_the_only_thing_that_needs_decoding() {
    // location@href and version@ver travel through the same attribute path.
    let xml = primary(
        r#"<package type="rpm">
  <name>escaped-location</name>
  <version epoch="0" ver="2&#x2e;1" rel="1"/>
  <location href="Packages/a&amp;b-1.0.rpm"/>
</package>"#,
    );

    let packages = parse(&xml).expect("parse failed");
    assert_eq!(
        packages[0].fields.get("href").map(String::as_str),
        Some("Packages/a&b-1.0.rpm")
    );
    assert_eq!(
        packages[0].fields.get("ver").map(String::as_str),
        Some("2.1")
    );
}

#[test]
fn an_unreadable_attribute_stops_the_parse_instead_of_vanishing() {
    // A bare `&` is not a legal entity reference. Skipping it silently would
    // emit a record that claims the source said something it did not.
    let xml = primary(
        r#"<package type="rpm">
  <name>broken</name>
  <version epoch="0" ver="1.0" rel="1"/>
  <format>
    <rpm:requires>
      <rpm:entry name="bad &amp ref"/>
    </rpm:requires>
  </format>
</package>"#,
    );

    let err = parse(&xml).expect_err("a malformed attribute must not be skipped");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("attribute"),
        "the error should name what failed: {err}"
    );
}

#[test]
fn an_entry_without_a_name_is_still_skipped_and_a_plain_one_still_parses() {
    let xml = primary(
        r#"<package type="rpm">
  <name>plain</name>
  <version epoch="0" ver="1.0" rel="1"/>
  <format>
    <rpm:requires>
      <rpm:entry flags="GE" epoch="0" ver="2.34"/>
      <rpm:entry name="glibc" flags="GE" epoch="0" ver="2.34" rel="100"/>
    </rpm:requires>
  </format>
</package>"#,
    );

    let packages = parse(&xml).expect("parse failed");
    assert_eq!(
        packages[0].deps.len(),
        1,
        "the nameless entry is not a dependency"
    );
    let dep = &packages[0].deps[0];
    assert_eq!(dep.name, "glibc");
    assert_eq!(dep.flags.as_deref(), Some("GE"));
    assert_eq!(dep.epoch.as_deref(), Some("0"));
    assert_eq!(dep.ver.as_deref(), Some("2.34"));
    assert_eq!(dep.rel.as_deref(), Some("100"));
}
