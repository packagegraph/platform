//! Shared error type for HTTP fetching operations across collectors.
//!
//! Cache I/O failures are handled internally by `HttpCache` and never
//! propagate through this type.

use std::fmt;

/// Errors that can occur during HTTP fetch operations.
#[derive(Debug)]
pub enum FetchError {
    /// Transport-level failure (DNS, TLS, connection reset, timeout).
    Transport { url: String, source: reqwest::Error },
    /// Server returned a non-success status code (not 404).
    HttpStatus { url: String, status: u16 },
    /// Server returned 404 Not Found.
    NotFound { url: String },
    /// A cached entry had an unexpected status code.
    UnexpectedCachedStatus { url: String, status: u16 },
    /// Response body could not be parsed (JSON, XML, etc.).
    Parse { url: String, detail: String },
    /// Response was structurally valid but semantically wrong.
    InvalidResponse { url: String, detail: String },
}

impl FetchError {
    /// Whether this error is worth retrying (transient failures).
    ///
    /// Returns `true` for transport errors and HTTP 429 / 5xx status codes.
    pub fn is_retryable(&self) -> bool {
        match self {
            FetchError::Transport { .. } => true,
            FetchError::HttpStatus { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }

    /// Classification string for grouping errors in end-of-run summaries.
    pub fn classification(&self) -> &'static str {
        match self {
            FetchError::Transport { .. } => "network",
            FetchError::HttpStatus { .. } => "http",
            FetchError::NotFound { .. } => "not_found",
            FetchError::Parse { .. } => "parse",
            FetchError::InvalidResponse { .. } => "parse",
            FetchError::UnexpectedCachedStatus { .. } => "cache",
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Transport { url, source } => {
                write!(f, "transport error fetching {}: {}", url, source)
            }
            FetchError::HttpStatus { url, status } => {
                write!(f, "HTTP {} from {}", status, url)
            }
            FetchError::NotFound { url } => {
                write!(f, "not found: {}", url)
            }
            FetchError::UnexpectedCachedStatus { url, status } => {
                write!(f, "unexpected cached status {} for {}", status, url)
            }
            FetchError::Parse { url, detail } => {
                write!(f, "parse error for {}: {}", url, detail)
            }
            FetchError::InvalidResponse { url, detail } => {
                write!(f, "invalid response from {}: {}", url, detail)
            }
        }
    }
}

impl std::error::Error for FetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FetchError::Transport { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_retryable_transport() {
        // Build a reqwest error by attempting to connect to an invalid URL
        let err = reqwest::blocking::get("http://[::0]:1/__invalid__").unwrap_err();
        let fe = FetchError::Transport {
            url: "http://example.com".into(),
            source: err,
        };
        assert!(fe.is_retryable());
        assert_eq!(fe.classification(), "network");
    }

    #[test]
    fn test_is_retryable_http_429() {
        let fe = FetchError::HttpStatus {
            url: "http://example.com".into(),
            status: 429,
        };
        assert!(fe.is_retryable());
        assert_eq!(fe.classification(), "http");
    }

    #[test]
    fn test_is_retryable_http_500() {
        let fe = FetchError::HttpStatus {
            url: "http://example.com".into(),
            status: 500,
        };
        assert!(fe.is_retryable());
    }

    #[test]
    fn test_is_retryable_http_503() {
        let fe = FetchError::HttpStatus {
            url: "http://example.com".into(),
            status: 503,
        };
        assert!(fe.is_retryable());
    }

    #[test]
    fn test_not_retryable_http_400() {
        let fe = FetchError::HttpStatus {
            url: "http://example.com".into(),
            status: 400,
        };
        assert!(!fe.is_retryable());
    }

    #[test]
    fn test_not_retryable_not_found() {
        let fe = FetchError::NotFound {
            url: "http://example.com/missing".into(),
        };
        assert!(!fe.is_retryable());
        assert_eq!(fe.classification(), "not_found");
    }

    #[test]
    fn test_not_retryable_parse() {
        let fe = FetchError::Parse {
            url: "http://example.com".into(),
            detail: "invalid JSON".into(),
        };
        assert!(!fe.is_retryable());
        assert_eq!(fe.classification(), "parse");
    }

    #[test]
    fn test_classification_invalid_response() {
        let fe = FetchError::InvalidResponse {
            url: "http://example.com".into(),
            detail: "missing field".into(),
        };
        assert_eq!(fe.classification(), "parse");
    }

    #[test]
    fn test_classification_unexpected_cached_status() {
        let fe = FetchError::UnexpectedCachedStatus {
            url: "http://example.com".into(),
            status: 301,
        };
        assert_eq!(fe.classification(), "cache");
        assert!(!fe.is_retryable());
    }

    #[test]
    fn test_display_format() {
        let fe = FetchError::HttpStatus {
            url: "http://example.com/api".into(),
            status: 503,
        };
        let msg = format!("{}", fe);
        assert!(msg.contains("503"));
        assert!(msg.contains("http://example.com/api"));
    }
}
