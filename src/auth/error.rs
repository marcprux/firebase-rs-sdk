use crate::app::AppError;
use crate::auth::types::MultiFactorError;
use crate::util::FirebaseError;
use std::borrow::Cow;
use std::fmt;

pub type AuthResult<T> = Result<T, AuthError>;

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuthError {
    Firebase(FirebaseError),
    App(AppError),
    /// The request never produced an HTTP response (DNS, TLS, timeout, serialization).
    ///
    /// Equivalent to the JS `auth/network-request-failed` code.
    Network(String),
    /// A credential or argument was rejected client-side before any request was made.
    InvalidCredential(String),
    NotImplemented(&'static str),
    MultiFactorRequired(MultiFactorError),
    MultiFactor(MultiFactorAuthError),
    /// The Identity Toolkit / Secure Token backend answered with an error, mapped to the same
    /// typed code the JS SDK would raise (see [`AuthErrorCode`]).
    Server(AuthServerError),
}

impl AuthError {
    /// Returns the typed error code when the error originated from the Firebase Auth backend.
    ///
    /// ```
    /// use firebase_rs_sdk::auth::{AuthError, AuthErrorCode};
    ///
    /// fn is_wrong_password(err: &AuthError) -> bool {
    ///     matches!(err.code(), Some(AuthErrorCode::WrongPassword | AuthErrorCode::InvalidCredential))
    /// }
    /// ```
    pub fn code(&self) -> Option<&AuthErrorCode> {
        match self {
            AuthError::Server(err) => Some(err.code()),
            _ => None,
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::Firebase(err) => write!(f, "{err}"),
            AuthError::App(err) => write!(f, "{err}"),
            AuthError::Network(message) => write!(f, "Network error: {message}"),
            AuthError::InvalidCredential(message) => write!(f, "Invalid credential: {message}"),
            AuthError::NotImplemented(feature) => write!(f, "{feature} is not implemented"),
            AuthError::MultiFactorRequired(err) => write!(f, "{err}"),
            AuthError::MultiFactor(err) => write!(f, "{err}"),
            AuthError::Server(err) => write!(f, "{err}"),
        }
    }
}

/// Typed error codes raised by the Firebase Auth backend.
///
/// Each variant corresponds to an `auth/...` code of the JS SDK's `AuthErrorCode`
/// (`packages/auth/src/core/errors.ts`). Server messages are translated with the same
/// `SERVER_ERROR_MAP` as `packages/auth/src/api/errors.ts`; messages without a dedicated
/// mapping are normalised the way the JS SDK does (lower-case, underscores to dashes) and
/// carried in [`AuthErrorCode::Other`], e.g. `configuration-not-found`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthErrorCode {
    /// `auth/admin-restricted-operation`: the operation (e.g. anonymous sign-in) is disabled.
    AdminRestrictedOperation,
    /// `auth/code-expired`: the SMS code expired.
    CodeExpired,
    /// `auth/credential-already-in-use`: the credential is linked to another account.
    CredentialAlreadyInUse,
    /// `auth/custom-token-mismatch`: the custom token belongs to a different project.
    CustomTokenMismatch,
    /// `auth/email-already-in-use`.
    EmailAlreadyInUse,
    /// `auth/expired-action-code`.
    ExpiredActionCode,
    /// `auth/internal-error`.
    InternalError,
    /// `auth/invalid-action-code`.
    InvalidActionCode,
    /// `auth/invalid-credential`: the supplied credential is malformed, expired or wrong.
    /// Projects with email enumeration protection return this for a wrong password too.
    InvalidCredential,
    /// `auth/invalid-custom-token`.
    InvalidCustomToken,
    /// `auth/invalid-email`.
    InvalidEmail,
    /// `auth/invalid-oauth-client-id`.
    InvalidOauthClientId,
    /// `auth/invalid-recaptcha-action`.
    InvalidRecaptchaAction,
    /// `auth/invalid-recaptcha-token`.
    InvalidRecaptchaToken,
    /// `auth/invalid-recaptcha-version`.
    InvalidRecaptchaVersion,
    /// `auth/invalid-req-type`.
    InvalidReqType,
    /// `auth/invalid-user-token`: the ID token is invalid, the user must sign in again.
    InvalidUserToken,
    /// `auth/invalid-verification-code`.
    InvalidVerificationCode,
    /// `auth/invalid-verification-id`.
    InvalidVerificationId,
    /// `auth/missing-android-pkg-name`.
    MissingAndroidPkgName,
    /// `auth/missing-client-type`.
    MissingClientType,
    /// `auth/missing-password`.
    MissingPassword,
    /// `auth/missing-recaptcha-token`.
    MissingRecaptchaToken,
    /// `auth/missing-recaptcha-version`.
    MissingRecaptchaVersion,
    /// `auth/missing-verification-id`.
    MissingVerificationId,
    /// `auth/network-request-failed`.
    NetworkRequestFailed,
    /// `auth/operation-not-allowed`: the sign-in provider is disabled in the console.
    OperationNotAllowed,
    /// `auth/password-does-not-meet-requirements`.
    PasswordDoesNotMeetRequirements,
    /// `auth/recaptcha-not-enabled`.
    RecaptchaNotEnabled,
    /// `auth/requires-recent-login`.
    RequiresRecentLogin,
    /// `auth/too-many-requests`.
    TooManyRequests,
    /// `auth/unauthorized-continue-uri`.
    UnauthorizedContinueUri,
    /// `auth/user-disabled`.
    UserDisabled,
    /// `auth/user-not-found`.
    UserNotFound,
    /// `auth/user-token-expired`: the refresh token is no longer valid.
    UserTokenExpired,
    /// `auth/invalid-refresh-token`: the stored refresh token was revoked or never existed.
    InvalidRefreshToken,
    /// `auth/missing-refresh-token`.
    MissingRefreshToken,
    /// `auth/wrong-password`.
    WrongPassword,
    /// Any other backend code, normalised to the JS form (e.g. `configuration-not-found`).
    Other(String),
}

impl AuthErrorCode {
    /// Returns the JS SDK code without the `auth/` prefix, e.g. `wrong-password`.
    pub fn as_str(&self) -> &str {
        match self {
            AuthErrorCode::AdminRestrictedOperation => "admin-restricted-operation",
            AuthErrorCode::CodeExpired => "code-expired",
            AuthErrorCode::CredentialAlreadyInUse => "credential-already-in-use",
            AuthErrorCode::CustomTokenMismatch => "custom-token-mismatch",
            AuthErrorCode::EmailAlreadyInUse => "email-already-in-use",
            AuthErrorCode::ExpiredActionCode => "expired-action-code",
            AuthErrorCode::InternalError => "internal-error",
            AuthErrorCode::InvalidActionCode => "invalid-action-code",
            AuthErrorCode::InvalidCredential => "invalid-credential",
            AuthErrorCode::InvalidCustomToken => "invalid-custom-token",
            AuthErrorCode::InvalidEmail => "invalid-email",
            AuthErrorCode::InvalidOauthClientId => "invalid-oauth-client-id",
            AuthErrorCode::InvalidRecaptchaAction => "invalid-recaptcha-action",
            AuthErrorCode::InvalidRecaptchaToken => "invalid-recaptcha-token",
            AuthErrorCode::InvalidRecaptchaVersion => "invalid-recaptcha-version",
            AuthErrorCode::InvalidReqType => "invalid-req-type",
            AuthErrorCode::InvalidUserToken => "invalid-user-token",
            AuthErrorCode::InvalidVerificationCode => "invalid-verification-code",
            AuthErrorCode::InvalidVerificationId => "invalid-verification-id",
            AuthErrorCode::MissingAndroidPkgName => "missing-android-pkg-name",
            AuthErrorCode::MissingClientType => "missing-client-type",
            AuthErrorCode::MissingPassword => "missing-password",
            AuthErrorCode::MissingRecaptchaToken => "missing-recaptcha-token",
            AuthErrorCode::MissingRecaptchaVersion => "missing-recaptcha-version",
            AuthErrorCode::MissingVerificationId => "missing-verification-id",
            AuthErrorCode::NetworkRequestFailed => "network-request-failed",
            AuthErrorCode::OperationNotAllowed => "operation-not-allowed",
            AuthErrorCode::PasswordDoesNotMeetRequirements => "password-does-not-meet-requirements",
            AuthErrorCode::RecaptchaNotEnabled => "recaptcha-not-enabled",
            AuthErrorCode::RequiresRecentLogin => "requires-recent-login",
            AuthErrorCode::TooManyRequests => "too-many-requests",
            AuthErrorCode::UnauthorizedContinueUri => "unauthorized-continue-uri",
            AuthErrorCode::UserDisabled => "user-disabled",
            AuthErrorCode::UserNotFound => "user-not-found",
            AuthErrorCode::UserTokenExpired => "user-token-expired",
            AuthErrorCode::InvalidRefreshToken => "invalid-refresh-token",
            AuthErrorCode::MissingRefreshToken => "missing-refresh-token",
            AuthErrorCode::WrongPassword => "wrong-password",
            AuthErrorCode::Other(code) => code.as_str(),
        }
    }

    /// Returns the fully qualified JS SDK code, e.g. `auth/wrong-password`.
    pub fn full_code(&self) -> String {
        format!("auth/{}", self.as_str())
    }

    /// Maps a raw Identity Toolkit / Secure Token server code (e.g. `INVALID_PASSWORD`) to the
    /// developer-facing code, mirroring `SERVER_ERROR_MAP` plus the special cases in
    /// `_performFetchWithErrorHandling` of the JS SDK.
    pub fn from_server_code(server_code: &str) -> Self {
        let normalized = normalize_error_code(server_code);
        match normalized.as_ref() {
            // Custom token errors.
            "INVALID_CUSTOM_TOKEN" => AuthErrorCode::InvalidCustomToken,
            "CREDENTIAL_MISMATCH" => AuthErrorCode::CustomTokenMismatch,
            "MISSING_CUSTOM_TOKEN" => AuthErrorCode::InternalError,
            // Create Auth URI errors.
            "INVALID_IDENTIFIER" => AuthErrorCode::InvalidEmail,
            "MISSING_CONTINUE_URI" => AuthErrorCode::InternalError,
            // Sign in with email and password errors (some apply to sign up too).
            "INVALID_PASSWORD" => AuthErrorCode::WrongPassword,
            "MISSING_PASSWORD" => AuthErrorCode::MissingPassword,
            "INVALID_LOGIN_CREDENTIALS" => AuthErrorCode::InvalidCredential,
            // Sign up with email and password errors.
            "EMAIL_EXISTS" => AuthErrorCode::EmailAlreadyInUse,
            "PASSWORD_LOGIN_DISABLED" => AuthErrorCode::OperationNotAllowed,
            // Verify assertion for sign in with credential errors.
            "INVALID_IDP_RESPONSE" | "INVALID_PENDING_TOKEN" | "INVALID_TEMPORARY_PROOF" => {
                AuthErrorCode::InvalidCredential
            }
            "FEDERATED_USER_ID_ALREADY_LINKED" => AuthErrorCode::CredentialAlreadyInUse,
            "MISSING_REQ_TYPE" => AuthErrorCode::InternalError,
            // Send password reset email errors.
            "EMAIL_NOT_FOUND" => AuthErrorCode::UserNotFound,
            "RESET_PASSWORD_EXCEED_LIMIT" | "TOO_MANY_ATTEMPTS_TRY_LATER" => AuthErrorCode::TooManyRequests,
            "EXPIRED_OOB_CODE" => AuthErrorCode::ExpiredActionCode,
            "INVALID_OOB_CODE" => AuthErrorCode::InvalidActionCode,
            "MISSING_OOB_CODE" => AuthErrorCode::InternalError,
            // Operations that require an ID token in the request.
            "CREDENTIAL_TOO_OLD_LOGIN_AGAIN" => AuthErrorCode::RequiresRecentLogin,
            "INVALID_ID_TOKEN" => AuthErrorCode::InvalidUserToken,
            "TOKEN_EXPIRED" | "USER_NOT_FOUND" => AuthErrorCode::UserTokenExpired,
            // The secure token endpoint reports a revoked or unknown refresh token this way.
            "INVALID_REFRESH_TOKEN" => AuthErrorCode::InvalidRefreshToken,
            "MISSING_REFRESH_TOKEN" => AuthErrorCode::MissingRefreshToken,
            "USER_DISABLED" => AuthErrorCode::UserDisabled,
            "PASSWORD_DOES_NOT_MEET_REQUIREMENTS" => AuthErrorCode::PasswordDoesNotMeetRequirements,
            // Phone auth errors.
            "INVALID_CODE" => AuthErrorCode::InvalidVerificationCode,
            "INVALID_SESSION_INFO" => AuthErrorCode::InvalidVerificationId,
            "MISSING_SESSION_INFO" => AuthErrorCode::MissingVerificationId,
            "SESSION_EXPIRED" => AuthErrorCode::CodeExpired,
            // Action code settings errors.
            "MISSING_ANDROID_PACKAGE_NAME" => AuthErrorCode::MissingAndroidPkgName,
            "UNAUTHORIZED_DOMAIN" => AuthErrorCode::UnauthorizedContinueUri,
            "INVALID_OAUTH_CLIENT_ID" => AuthErrorCode::InvalidOauthClientId,
            // General backend errors.
            "ADMIN_ONLY_OPERATION" => AuthErrorCode::AdminRestrictedOperation,
            "BLOCKING_FUNCTION_ERROR_RESPONSE" => AuthErrorCode::InternalError,
            "RECAPTCHA_NOT_ENABLED" => AuthErrorCode::RecaptchaNotEnabled,
            "MISSING_RECAPTCHA_TOKEN" => AuthErrorCode::MissingRecaptchaToken,
            "INVALID_RECAPTCHA_TOKEN" => AuthErrorCode::InvalidRecaptchaToken,
            "INVALID_RECAPTCHA_ACTION" => AuthErrorCode::InvalidRecaptchaAction,
            "MISSING_CLIENT_TYPE" => AuthErrorCode::MissingClientType,
            "MISSING_RECAPTCHA_VERSION" => AuthErrorCode::MissingRecaptchaVersion,
            "INVALID_RECAPTCHA_VERSION" => AuthErrorCode::InvalidRecaptchaVersion,
            "INVALID_REQ_TYPE" => AuthErrorCode::InvalidReqType,
            "NETWORK_REQUEST_FAILED" => AuthErrorCode::NetworkRequestFailed,
            other => {
                // JS: serverErrorCode.toLowerCase().replace(/[_\s]+/g, '-')
                let mut code = String::with_capacity(other.len());
                let mut pending_dash = false;
                for ch in other.chars() {
                    if ch == '_' || ch.is_whitespace() || ch == '-' {
                        pending_dash = !code.is_empty();
                    } else {
                        if pending_dash {
                            code.push('-');
                            pending_dash = false;
                        }
                        code.push(ch.to_ascii_lowercase());
                    }
                }
                if code.is_empty() {
                    AuthErrorCode::InternalError
                } else {
                    AuthErrorCode::Other(code)
                }
            }
        }
    }
}

impl fmt::Display for AuthErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "auth/{}", self.as_str())
    }
}

/// An error reported by the Firebase Auth backend (Identity Toolkit or Secure Token API).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthServerError {
    code: AuthErrorCode,
    server_code: String,
    server_message: Option<String>,
    http_status: Option<u16>,
}

impl AuthServerError {
    /// Builds an error from the raw server code (e.g. `INVALID_PASSWORD`), an optional detail
    /// message and the HTTP status of the response.
    pub fn new(server_code: impl Into<String>, server_message: Option<String>, http_status: Option<u16>) -> Self {
        let server_code = server_code.into();
        Self {
            code: AuthErrorCode::from_server_code(&server_code),
            server_code,
            server_message,
            http_status,
        }
    }

    /// The typed, developer-facing code (`auth/...`).
    pub fn code(&self) -> &AuthErrorCode {
        &self.code
    }

    /// The raw code string sent by the backend, e.g. `INVALID_PASSWORD` or `CONFIGURATION_NOT_FOUND`.
    pub fn server_code(&self) -> &str {
        &self.server_code
    }

    /// Additional detail the backend appended after the code (`CODE : detail`), if any.
    pub fn server_message(&self) -> Option<&str> {
        self.server_message.as_deref()
    }

    /// HTTP status of the response that carried the error, when known.
    pub fn http_status(&self) -> Option<u16> {
        self.http_status
    }
}

impl fmt::Display for AuthServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.code, self.server_code)?;
        if let Some(message) = self.server_message() {
            if !message.is_empty() {
                write!(f, ": {message}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for AuthServerError {}

/// Converts a non-successful Identity Toolkit / Secure Token response into an [`AuthError`].
///
/// Mirrors `_performFetchWithErrorHandling` in `packages/auth/src/api/index.ts`: the error
/// message is split on ` : ` into a server code and optional detail, multi-factor codes keep
/// their dedicated [`AuthError::MultiFactor`] representation, and everything else becomes an
/// [`AuthError::Server`] with a typed [`AuthErrorCode`].
pub(crate) fn map_server_error(http_status: Option<u16>, body: &str) -> AuthError {
    let message = extract_server_message(body);
    let Some(message) = message else {
        let snippet: String = body.trim().chars().take(200).collect();
        let detail = match (http_status, snippet.is_empty()) {
            (Some(status), true) => format!("HTTP {status} without a Firebase error body"),
            (Some(status), false) => format!("HTTP {status}: {snippet}"),
            (None, true) => "response without a Firebase error body".to_string(),
            (None, false) => snippet,
        };
        return AuthError::Server(AuthServerError {
            code: AuthErrorCode::InternalError,
            server_code: "INTERNAL_ERROR".to_string(),
            server_message: Some(detail),
            http_status,
        });
    };

    if let Some(mapped) = map_mfa_error_code(&message) {
        return mapped;
    }

    let (server_code, detail) = match message.split_once(" : ") {
        Some((code, rest)) => (code.trim().to_string(), Some(rest.trim().to_string())),
        None => (message.trim().to_string(), None),
    };
    AuthError::Server(AuthServerError::new(server_code, detail, http_status))
}

/// Pulls `error.message` (error responses) or `errorMessage` (some 200 responses) out of a body.
fn extract_server_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|m| m.as_str())
        .or_else(|| value.get("errorMessage").and_then(|m| m.as_str()))
        .map(str::to_string)
        .filter(|m| !m.is_empty())
}

impl std::error::Error for AuthError {}

impl From<FirebaseError> for AuthError {
    fn from(error: FirebaseError) -> Self {
        AuthError::Firebase(error)
    }
}

impl From<AppError> for AuthError {
    fn from(error: AppError) -> Self {
        AuthError::App(error)
    }
}

/// Enumerates multi-factor specific error categories surfaced by Firebase Auth.
///
/// Mirrors the JavaScript [`AuthErrorCode`](https://github.com/firebase/firebase-js-sdk/blob/HEAD/packages/auth/src/core/errors.ts)
/// variants related to multi-factor authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiFactorAuthErrorCode {
    /// The provided multi-factor session (pending credential) is missing from the request.
    MissingSession,
    /// The provided multi-factor session (pending credential) is invalid or expired.
    InvalidSession,
    /// Required multi-factor enrollment data (e.g. enrollment ID) is missing from the request.
    MissingInfo,
    /// The requested multi-factor enrollment could not be found for the current user.
    InfoNotFound,
    /// A multi-factor challenge must be completed before the operation can continue.
    ChallengeRequired,
}

impl MultiFactorAuthErrorCode {
    fn default_message(self) -> &'static str {
        match self {
            MultiFactorAuthErrorCode::MissingSession => "Multi-factor session is required to continue the challenge",
            MultiFactorAuthErrorCode::InvalidSession => "The supplied multi-factor session is no longer valid",
            MultiFactorAuthErrorCode::MissingInfo => "Required multi-factor enrollment information is missing",
            MultiFactorAuthErrorCode::InfoNotFound => "The requested multi-factor enrollment could not be found",
            MultiFactorAuthErrorCode::ChallengeRequired => "Multi-factor challenge required to complete the operation",
        }
    }
}

/// Represents a typed multi-factor error emitted by Firebase Auth REST endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiFactorAuthError {
    code: MultiFactorAuthErrorCode,
    server_message: Option<String>,
}

impl MultiFactorAuthError {
    /// Creates a new error with the provided code and optional server-supplied message detail.
    pub fn new(code: MultiFactorAuthErrorCode, server_message: Option<String>) -> Self {
        Self { code, server_message }
    }

    /// Returns the structured multi-factor error code.
    pub fn code(&self) -> MultiFactorAuthErrorCode {
        self.code
    }

    /// Returns the raw server message if Firebase sent extra context.
    pub fn server_message(&self) -> Option<&str> {
        self.server_message.as_deref()
    }
}

impl fmt::Display for MultiFactorAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let base = self.code.default_message();
        match self.server_message() {
            Some(detail) if !detail.is_empty() && detail != base => {
                write!(f, "{base} (server message: {detail})")
            }
            _ => write!(f, "{base}"),
        }
    }
}

/// Attempts to convert a REST error message into a typed multi-factor [`AuthError`].
pub(crate) fn map_mfa_error_code(message: &str) -> Option<AuthError> {
    let (raw_code, detail) = split_error_message(message);
    let normalized = normalize_error_code(raw_code);

    let code = match normalized.as_ref() {
        "INVALID_MFA_SESSION"
        | "INVALID_MFA_PENDING_CREDENTIAL"
        | "INVALID_MULTI_FACTOR_SESSION"
        | "INVALID_MULTI_FACTOR_PENDING_CREDENTIAL" => MultiFactorAuthErrorCode::InvalidSession,
        "MISSING_MFA_SESSION"
        | "MISSING_MFA_PENDING_CREDENTIAL"
        | "MISSING_MULTI_FACTOR_SESSION"
        | "MISSING_MULTI_FACTOR_PENDING_CREDENTIAL" => MultiFactorAuthErrorCode::MissingSession,
        "MISSING_MFA_INFO"
        | "MISSING_MFA_ENROLLMENT_ID"
        | "MISSING_MULTI_FACTOR_INFO"
        | "MISSING_MULTI_FACTOR_ENROLLMENT_ID" => MultiFactorAuthErrorCode::MissingInfo,
        "MFA_INFO_NOT_FOUND"
        | "MFA_ENROLLMENT_NOT_FOUND"
        | "MULTI_FACTOR_INFO_NOT_FOUND"
        | "MULTI_FACTOR_ENROLLMENT_NOT_FOUND" => MultiFactorAuthErrorCode::InfoNotFound,
        "MFA_REQUIRED" | "MULTI_FACTOR_AUTH_REQUIRED" | "AUTH/MULTI-FACTOR-AUTH-REQUIRED" => {
            MultiFactorAuthErrorCode::ChallengeRequired
        }
        _ => return None,
    };

    let server_message = detail.map(|value| value.to_string()).or_else(|| {
        if raw_code.is_empty() {
            None
        } else {
            Some(raw_code.to_string())
        }
    });
    Some(AuthError::MultiFactor(MultiFactorAuthError::new(code, server_message)))
}

fn split_error_message(message: &str) -> (&str, Option<&str>) {
    match message.split_once(':') {
        Some((code, rest)) => (code.trim(), Some(rest.trim())),
        None => (message.trim(), None),
    }
}

fn normalize_error_code(code: &str) -> Cow<'_, str> {
    let stripped = code.trim();
    let without_prefix = stripped.strip_prefix("auth/").unwrap_or(stripped);
    if without_prefix.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
        Cow::Borrowed(without_prefix)
    } else {
        let mut candidate = without_prefix
            .chars()
            .map(|ch| match ch {
                '-' => '_',
                '/' => '_',
                _ => ch,
            })
            .collect::<String>();
        candidate.make_ascii_uppercase();
        Cow::Owned(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_codes_map_like_the_js_sdk() {
        let cases = [
            ("INVALID_PASSWORD", AuthErrorCode::WrongPassword),
            ("INVALID_LOGIN_CREDENTIALS", AuthErrorCode::InvalidCredential),
            ("EMAIL_NOT_FOUND", AuthErrorCode::UserNotFound),
            ("EMAIL_EXISTS", AuthErrorCode::EmailAlreadyInUse),
            ("USER_DISABLED", AuthErrorCode::UserDisabled),
            ("TOO_MANY_ATTEMPTS_TRY_LATER", AuthErrorCode::TooManyRequests),
            ("TOKEN_EXPIRED", AuthErrorCode::UserTokenExpired),
            ("USER_NOT_FOUND", AuthErrorCode::UserTokenExpired),
            ("CREDENTIAL_TOO_OLD_LOGIN_AGAIN", AuthErrorCode::RequiresRecentLogin),
            ("ADMIN_ONLY_OPERATION", AuthErrorCode::AdminRestrictedOperation),
            ("PASSWORD_LOGIN_DISABLED", AuthErrorCode::OperationNotAllowed),
            ("FEDERATED_USER_ID_ALREADY_LINKED", AuthErrorCode::CredentialAlreadyInUse),
            ("INVALID_OOB_CODE", AuthErrorCode::InvalidActionCode),
            ("SESSION_EXPIRED", AuthErrorCode::CodeExpired),
            ("MISSING_CUSTOM_TOKEN", AuthErrorCode::InternalError),
        ];
        for (server, expected) in cases {
            assert_eq!(AuthErrorCode::from_server_code(server), expected, "{server}");
        }
        assert_eq!(
            AuthErrorCode::from_server_code("CONFIGURATION_NOT_FOUND"),
            AuthErrorCode::Other("configuration-not-found".into())
        );
        assert_eq!(
            AuthErrorCode::from_server_code("OPERATION_NOT_ALLOWED"),
            AuthErrorCode::Other("operation-not-allowed".into())
        );
        assert_eq!(AuthErrorCode::WrongPassword.full_code(), "auth/wrong-password");
        assert_eq!(AuthErrorCode::WrongPassword.to_string(), "auth/wrong-password");
    }

    #[test]
    fn map_server_error_splits_code_and_detail() {
        let body = r#"{"error":{"code":400,"message":"TOO_MANY_ATTEMPTS_TRY_LATER : Access to this account has been temporarily disabled.","errors":[]}}"#;
        match map_server_error(Some(400), body) {
            AuthError::Server(err) => {
                assert_eq!(err.code(), &AuthErrorCode::TooManyRequests);
                assert_eq!(err.server_code(), "TOO_MANY_ATTEMPTS_TRY_LATER");
                assert_eq!(
                    err.server_message(),
                    Some("Access to this account has been temporarily disabled.")
                );
                assert_eq!(err.http_status(), Some(400));
                assert!(err
                    .to_string()
                    .starts_with("auth/too-many-requests (TOO_MANY_ATTEMPTS_TRY_LATER)"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn map_server_error_reads_error_message_field() {
        let body = r#"{"errorMessage":"INVALID_PASSWORD"}"#;
        let err = map_server_error(Some(200), body);
        assert_eq!(err.code(), Some(&AuthErrorCode::WrongPassword));
    }

    #[test]
    fn map_server_error_keeps_multi_factor_mapping() {
        let body = r#"{"error":{"message":"MISSING_MFA_PENDING_CREDENTIAL"}}"#;
        assert!(matches!(map_server_error(Some(400), body), AuthError::MultiFactor(_)));
    }

    #[test]
    fn map_server_error_without_json_body_is_internal() {
        let err = map_server_error(Some(502), "<html>Bad Gateway</html>");
        match err {
            AuthError::Server(err) => {
                assert_eq!(err.code(), &AuthErrorCode::InternalError);
                assert_eq!(err.http_status(), Some(502));
                assert!(err.server_message().unwrap().contains("HTTP 502"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn map_mfa_error_code_handles_pending_credential() {
        let error = map_mfa_error_code("MISSING_MFA_PENDING_CREDENTIAL");
        match error {
            Some(AuthError::MultiFactor(err)) => {
                assert_eq!(err.code(), MultiFactorAuthErrorCode::MissingSession);
                assert_eq!(err.server_message(), Some("MISSING_MFA_PENDING_CREDENTIAL"));
            }
            other => panic!("unexpected mapping result: {other:?}"),
        }
    }

    #[test]
    fn map_mfa_error_code_accepts_auth_prefixed_values() {
        let error = map_mfa_error_code("auth/multi-factor-info-not-found");
        match error {
            Some(AuthError::MultiFactor(err)) => {
                assert_eq!(err.code(), MultiFactorAuthErrorCode::InfoNotFound);
            }
            other => panic!("unexpected mapping result: {other:?}"),
        }
    }

    #[test]
    fn map_mfa_error_code_returns_none_for_unknown_codes() {
        assert!(map_mfa_error_code("SOME_OTHER_ERROR").is_none());
    }

    #[test]
    fn map_mfa_error_code_handles_challenge_required() {
        let error = map_mfa_error_code("auth/multi-factor-auth-required");
        match error {
            Some(AuthError::MultiFactor(err)) => {
                assert_eq!(err.code(), MultiFactorAuthErrorCode::ChallengeRequired);
            }
            other => panic!("unexpected mapping result: {other:?}"),
        }
    }
}
