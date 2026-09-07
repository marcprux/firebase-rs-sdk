pub fn is_url(path: &str) -> bool {
    if let Some(index) = path.find("://") {
        path[..index].chars().all(|ch| ch.is_ascii_alphabetic()) && index > 0
    } else {
        false
    }
}

pub fn is_retry_status_code(status: u16, additional: &[u16]) -> bool {
    (500..600).contains(&status) || matches!(status, 408 | 429) || additional.contains(&status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_urls() {
        assert!(is_url("gs://bucket/path"));
        assert!(is_url("https://example.com"));
        assert!(!is_url("not/a/url"));
        assert!(!is_url("://missing"));
    }

    #[test]
    fn retry_status_codes() {
        assert!(is_retry_status_code(500, &[]));
        assert!(is_retry_status_code(408, &[]));
        assert!(!is_retry_status_code(404, &[]));
        assert!(is_retry_status_code(499, &[499]));
    }
}

/// Characters `encodeURIComponent` leaves untouched besides ASCII letters and digits.
const URI_COMPONENT_SAFE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// Percent-encodes `value` exactly like JavaScript's `encodeURIComponent`, which is what the
/// Storage REST API and the JS SDK use for bucket names, object paths and download tokens.
pub fn encode_uri_component(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, URI_COMPONENT_SAFE).to_string()
}

#[cfg(test)]
mod uri_tests {
    use super::encode_uri_component;

    #[test]
    fn matches_encode_uri_component() {
        assert_eq!(encode_uri_component("my-bucket.appspot.com"), "my-bucket.appspot.com");
        assert_eq!(encode_uri_component("photos/cat 1.png"), "photos%2Fcat%201.png");
        assert_eq!(encode_uri_component("a_b~c!d*e'f(g)h"), "a_b~c!d*e'f(g)h");
        assert_eq!(encode_uri_component("ü+&="), "%C3%BC%2B%26%3D");
    }
}
