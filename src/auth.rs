//! SMTP AUTH LOGIN and AUTH PLAIN decoding/authentication.
//! Semantics intentionally match Authenticator.__call__ in server.py:
//! only LOGIN/PLAIN can authenticate; correct exact UTF-8 credentials succeed;
//! all other input is an auth failure (server presents its normal 535 response).

use base64::{engine::general_purpose::STANDARD, Engine};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResult {
    Success,
    Failure,
    /// AUTH mechanism other than LOGIN/PLAIN was requested. aiosmtpd's
    /// Authenticator in the Python original only registers LOGIN/PLAIN
    /// handlers, so any other mechanism never reaches user code and the
    /// SMTP server itself replies with "504 5.5.4 Unrecognized authentication
    /// type". This variant documents that case for callers/tests even though
    /// our own AUTH mechanism list currently mirrors that restriction and
    /// never constructs it directly.
    #[allow(dead_code)]
    Unsupported,
}

pub fn validate_credentials(
    username: &str,
    password: &str,
    expected_user: &str,
    expected_pass: &str,
) -> AuthResult {
    if username == expected_user && password == expected_pass {
        AuthResult::Success
    } else {
        AuthResult::Failure
    }
}

pub fn auth_plain(encoded: &str, expected_user: &str, expected_pass: &str) -> AuthResult {
    let decoded = match STANDARD.decode(encoded.trim()) {
        Ok(v) => v,
        Err(_) => return AuthResult::Failure,
    };
    let fields: Vec<&[u8]> = decoded.split(|b| *b == 0).collect();
    // RFC 4616 is [authzid] NUL authcid NUL password. Python aiosmtpd's
    // LoginPassword exposes authcid/password, so ignore authzid too.
    if fields.len() != 3 {
        return AuthResult::Failure;
    }
    let user = match std::str::from_utf8(fields[1]) {
        Ok(s) => s,
        Err(_) => return AuthResult::Failure,
    };
    let pass = match std::str::from_utf8(fields[2]) {
        Ok(s) => s,
        Err(_) => return AuthResult::Failure,
    };
    validate_credentials(user, pass, expected_user, expected_pass)
}

pub fn decode_login_step(encoded: &str) -> Result<String, AuthResult> {
    let bytes = STANDARD
        .decode(encoded.trim())
        .map_err(|_| AuthResult::Failure)?;
    String::from_utf8(bytes).map_err(|_| AuthResult::Failure)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_success_and_failure() {
        assert_eq!(
            validate_credentials("relay", "secret", "relay", "secret"),
            AuthResult::Success
        );
        assert_eq!(
            validate_credentials("relay", "nope", "relay", "secret"),
            AuthResult::Failure
        );
    }

    #[test]
    fn plain_success_and_failure() {
        let good = STANDARD.encode(b"\0relay\0secret");
        let bad = STANDARD.encode(b"\0relay\0wrong");
        assert_eq!(auth_plain(&good, "relay", "secret"), AuthResult::Success);
        assert_eq!(auth_plain(&bad, "relay", "secret"), AuthResult::Failure);
        assert_eq!(
            auth_plain("not base64", "relay", "secret"),
            AuthResult::Failure
        );
    }
}
