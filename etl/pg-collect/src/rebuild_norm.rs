//! Anchored NVR normalization for rebuild-fidelity classification.
use once_cell::sync::Lazy;
use regex::Regex;

// Anchored vendor suffixes observed in Alma/Rocky (see spike report).
// Only strip a vendor token (.rocky/.alma) optionally followed by dotted numbers,
// anchored at end. Never strip a bare trailing .N.M with no vendor token.
static VENDOR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\.(?:rocky|alma)(?:\.\d+)*$").unwrap());

/// Strip an anchored vendor suffix from an NVR's release. Returns the input
/// unchanged when no anchored vendor token is present.
pub fn strip_vendor(nvr: &str) -> String {
    VENDOR_RE.replace(nvr, "").into_owned()
}

/// Truncate an NVR at the first `.module` marker (`.module+el9…` or
/// `.module_el9…`), collapsing vendor-specific module build-context.
pub fn module_base(nvr: &str) -> String {
    match nvr.find(".module") {
        Some(i) => nvr[..i].to_string(),
        None => nvr.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_vendor_removes_only_anchored_suffixes() {
        assert_eq!(strip_vendor("24.4-8.el9.rocky.0.1"), "24.4-8.el9");
        assert_eq!(strip_vendor("34.25.7.14-1.el9.rocky.0.6"), "34.25.7.14-1.el9");
        assert_eq!(strip_vendor("3.0.7-27.el9.alma.1"), "3.0.7-27.el9");
        // must NOT strip a legitimate release component
        assert_eq!(strip_vendor("11-13.el9.0.1"), "11-13.el9.0.1");
        assert_eq!(strip_vendor("3.5.5-6.el9_8"), "3.5.5-6.el9_8");
    }

    #[test]
    fn module_base_truncates_at_module_marker() {
        assert_eq!(module_base("1.9.0-4.module+el9.8.0+24092+eb9f67d0"), "1.9.0-4");
        assert_eq!(module_base("1.9.0-4.module_el9.6.0+148+fb6dc857"), "1.9.0-4");
        assert_eq!(module_base("3.5.5-6.el9_8"), "3.5.5-6.el9_8");
    }
}
