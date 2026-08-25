//! Shared version classifier for Maven artifact versions.
//!
//! Classifies version strings into categories that drive cache TTL
//! decisions and dependency emission policies.

use regex::Regex;
use std::sync::LazyLock;

/// Timestamped snapshot pattern: `1.0-20260101.120000-1`
static TIMESTAMPED_SNAPSHOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-\d{8}\.\d{6}-\d+$").expect("valid regex"));

/// Classification of a Maven version string.
#[derive(Debug, Clone, PartialEq)]
pub enum VersionClass {
    /// A normal, concrete release version (e.g. `1.2.3`, `2.0.0-beta1`).
    ConcreteVersion(String),
    /// A snapshot version (e.g. `1.0-SNAPSHOT`, `1.0-20260101.120000-1`).
    Snapshot(String),
    /// A Maven version range (e.g. `[1.0,2.0)`, `(,1.5]`).
    VersionRange(String),
    /// Contains unresolved `${...}` property references.
    UnresolvedProperty(String),
    /// Special Maven token (`LATEST` or `RELEASE`).
    SpecialToken(String),
    /// No version specified.
    NoVersion,
}

/// Classify a Maven version string.
///
/// Detection rules (evaluated in order):
/// 1. `None` → `NoVersion`
/// 2. Contains `${` → `UnresolvedProperty`
/// 3. Starts with `[` or `(` → `VersionRange`
/// 4. Equals `LATEST` or `RELEASE` → `SpecialToken`
/// 5. Ends with `-SNAPSHOT` or matches timestamped snapshot → `Snapshot`
/// 6. Otherwise → `ConcreteVersion`
pub fn classify_version(version: Option<&str>) -> VersionClass {
    let v = match version {
        Some(s) if !s.is_empty() => s,
        _ => return VersionClass::NoVersion,
    };

    if v.contains("${") {
        return VersionClass::UnresolvedProperty(v.to_string());
    }

    if v.starts_with('[') || v.starts_with('(') {
        return VersionClass::VersionRange(v.to_string());
    }

    if v == "LATEST" || v == "RELEASE" {
        return VersionClass::SpecialToken(v.to_string());
    }

    if v.ends_with("-SNAPSHOT") || TIMESTAMPED_SNAPSHOT.is_match(v) {
        return VersionClass::Snapshot(v.to_string());
    }

    VersionClass::ConcreteVersion(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_concrete_version() {
        assert_eq!(
            classify_version(Some("1.2.3")),
            VersionClass::ConcreteVersion("1.2.3".to_string())
        );
    }

    #[test]
    fn test_concrete_prerelease() {
        assert_eq!(
            classify_version(Some("2.0.0-beta1")),
            VersionClass::ConcreteVersion("2.0.0-beta1".to_string())
        );
    }

    #[test]
    fn test_snapshot() {
        assert_eq!(
            classify_version(Some("1.0-SNAPSHOT")),
            VersionClass::Snapshot("1.0-SNAPSHOT".to_string())
        );
    }

    #[test]
    fn test_timestamped_snapshot() {
        assert_eq!(
            classify_version(Some("1.0-20260101.120000-1")),
            VersionClass::Snapshot("1.0-20260101.120000-1".to_string())
        );
    }

    #[test]
    fn test_version_range_bracket() {
        assert_eq!(
            classify_version(Some("[1.0,2.0)")),
            VersionClass::VersionRange("[1.0,2.0)".to_string())
        );
    }

    #[test]
    fn test_version_range_paren() {
        assert_eq!(
            classify_version(Some("(,1.5]")),
            VersionClass::VersionRange("(,1.5]".to_string())
        );
    }

    #[test]
    fn test_unresolved_property() {
        assert_eq!(
            classify_version(Some("${parent.version}")),
            VersionClass::UnresolvedProperty("${parent.version}".to_string())
        );
    }

    #[test]
    fn test_special_token_latest() {
        assert_eq!(
            classify_version(Some("LATEST")),
            VersionClass::SpecialToken("LATEST".to_string())
        );
    }

    #[test]
    fn test_special_token_release() {
        assert_eq!(
            classify_version(Some("RELEASE")),
            VersionClass::SpecialToken("RELEASE".to_string())
        );
    }

    #[test]
    fn test_no_version_none() {
        assert_eq!(classify_version(None), VersionClass::NoVersion);
    }

    #[test]
    fn test_no_version_empty() {
        assert_eq!(classify_version(Some("")), VersionClass::NoVersion);
    }

    #[test]
    fn test_concrete_with_qualifier() {
        assert_eq!(
            classify_version(Some("3.14.0-jre")),
            VersionClass::ConcreteVersion("3.14.0-jre".to_string())
        );
    }

    #[test]
    fn test_unresolved_in_middle() {
        assert_eq!(
            classify_version(Some("1.0-${qualifier}")),
            VersionClass::UnresolvedProperty("1.0-${qualifier}".to_string())
        );
    }

    #[test]
    fn test_timestamped_snapshot_multidigit_build() {
        assert_eq!(
            classify_version(Some("2.5.0-20260315.093012-42")),
            VersionClass::Snapshot("2.5.0-20260315.093012-42".to_string())
        );
    }
}
