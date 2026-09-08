use std::time::{Duration, SystemTime};

/// How long before its stated expiry a token is treated as expired.
///
/// The JS SDK refreshes an hour early (`TOKEN_EXPIRATION_BUFFER` in
/// `packages/installations/src/util/constants.ts`): a token handed to a caller has to outlive the
/// request it is about to authorise, and the backend's clock is not ours.
pub const TOKEN_EXPIRATION_BUFFER: Duration = Duration::from_secs(60 * 60);

/// Represents an authentication token produced by the Firebase Installations service.
///
/// Mirrors the JavaScript type defined in
/// `packages/installations/src/interfaces/installation-entry.ts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallationToken {
    pub token: String,
    pub expires_at: SystemTime,
}

impl InstallationToken {
    /// Returns `true` when the token is expired, or close enough to it that handing it out would
    /// risk a request failing halfway through: see [`TOKEN_EXPIRATION_BUFFER`].
    pub fn is_expired(&self) -> bool {
        self.expires_within(TOKEN_EXPIRATION_BUFFER)
    }

    /// Returns `true` when the token expires within `buffer` of now.
    ///
    /// Strictly `<`, as `isAuthTokenExpired` in
    /// `packages/installations/src/helpers/refresh-auth-token.ts` is: a token whose lifetime is
    /// exactly the buffer is still usable at the moment it is minted.
    pub fn expires_within(&self, buffer: Duration) -> bool {
        self.expires_at < SystemTime::now() + buffer
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;

    #[test]
    fn a_token_about_to_expire_counts_as_expired() {
        // The JS SDK refreshes an hour early; a token with 30 minutes left is not worth handing to
        // a caller, because the request it authorises may outlive it.
        let almost_gone = InstallationToken {
            token: "t".into(),
            expires_at: SystemTime::now() + Duration::from_secs(30 * 60),
        };
        assert!(almost_gone.is_expired());

        let fresh = InstallationToken {
            token: "t".into(),
            expires_at: SystemTime::now() + Duration::from_secs(2 * 60 * 60),
        };
        assert!(!fresh.is_expired());
    }
}

/// Public data describing a cached Firebase Installation entry.
///
/// This mirrors the Installation entry returned by the JS SDK (`InstallationEntry`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallationEntryData {
    pub fid: String,
    pub refresh_token: String,
    pub auth_token: InstallationToken,
}
