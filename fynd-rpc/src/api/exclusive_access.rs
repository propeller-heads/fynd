//! Resolves the caller's access to exclusive liquidity from the request headers.

use actix_web::http::header::HeaderMap;
use fynd_core::ExclusiveAccess;

/// Header through which the authenticating proxy grants access to exclusive liquidity.
const EXCLUSIVE_ACCESS_HEADER: &str = "x-exclusive-access";

/// Reads the exclusive access the authenticating proxy resolved for this request.
///
/// Only the exact value `true` (case-insensitive) grants access; absent, malformed, and all other
/// values deny it. Fynd does not authenticate callers itself, so this header is only meaningful
/// when the server is unreachable except through that proxy — otherwise a caller can set it
/// themselves.
pub(crate) fn from_headers(headers: &HeaderMap) -> ExclusiveAccess {
    let granted = headers
        .get(EXCLUSIVE_ACCESS_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));

    if granted {
        ExclusiveAccess::Granted
    } else {
        ExclusiveAccess::Denied
    }
}

#[cfg(test)]
mod tests {
    use actix_web::http::header::{HeaderName, HeaderValue};
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::absent(None, ExclusiveAccess::Denied)]
    #[case::lowercase_true(Some("true"), ExclusiveAccess::Granted)]
    #[case::uppercase_true(Some("TRUE"), ExclusiveAccess::Granted)]
    #[case::explicit_false(Some("false"), ExclusiveAccess::Denied)]
    #[case::empty(Some(""), ExclusiveAccess::Denied)]
    #[case::numeric_one(Some("1"), ExclusiveAccess::Denied)]
    #[case::whitespace_padded(Some(" true "), ExclusiveAccess::Denied)]
    fn test_from_headers(#[case] header_value: Option<&str>, #[case] expected: ExclusiveAccess) {
        let mut headers = HeaderMap::new();
        if let Some(value) = header_value {
            headers.insert(
                HeaderName::from_static(EXCLUSIVE_ACCESS_HEADER),
                HeaderValue::from_str(value).expect("test header value is valid"),
            );
        }

        assert_eq!(from_headers(&headers), expected);
    }
}
