//! The three checks every `serve` request passes before routing, also applied to the WISP relay
//! and bridge sockets:
//!
//! 1. the bearer token from the owner-only token file in the runtime role, re-checked on every
//!    read;
//! 2. `Host` must be `127.0.0.1:<port>` or `localhost:<port>` (DNS-rebinding defense);
//! 3. `Origin`, when present, must equal one of those origins. There is no CORS, so a cross-site
//!    page can read no response and a cross-site WebSocket is refused outright.

use std::fmt;

use crate::paths::{GuardError, OwnerOnlyFiles};

/// Bytes of a daemon token: 256 bits of OS entropy.
pub const TOKEN_BYTES: usize = 32;

pub const TOKEN_FILE: &str = "token";

/// The `Authorization` scheme, lowercase for a case-insensitive comparison.
const BEARER: &str = "bearer";

/// A 256-bit daemon token. It grants arbitrary firmware execution, so it never reaches `Debug`,
/// `Display` or a log, and [`Token::matches`] compares in constant time.
#[derive(Clone, PartialEq, Eq)]
pub struct Token([u8; TOKEN_BYTES]);

impl Token {
    pub fn from_bytes(bytes: [u8; TOKEN_BYTES]) -> Token {
        Token(bytes)
    }

    /// A fresh token from OS entropy (`getrandom`, the OS generator on both hosts).
    pub fn generate() -> Result<Token, TokenError> {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| TokenError::NoEntropy)?;
        Ok(Token(bytes))
    }

    /// The token as 64 lowercase hex characters, as the token file and header carry it.
    pub fn to_hex(&self) -> String {
        pemu_loader::hex(&self.0)
    }

    /// Parses the hex form. Any other length is refused, so a truncated file is an error rather
    /// than a short token.
    pub fn parse_hex(text: &str) -> Result<Token, TokenError> {
        pemu_loader::parse_sha256_hex(text.trim())
            .map(Token)
            .ok_or(TokenError::Malformed)
    }

    /// Constant-time equality: a localhost attacker can time a comparison.
    pub fn matches(&self, other: &Token) -> bool {
        let mut diff = 0u8;
        for i in 0..TOKEN_BYTES {
            diff |= self.0[i] ^ other.0[i];
        }
        diff == 0
    }

    /// Writes the token owner-only into `dir/token`.
    pub fn store(
        &self,
        files: &OwnerOnlyFiles<'_>,
        dir: &std::path::Path,
    ) -> Result<(), GuardError> {
        files.write(&dir.join(TOKEN_FILE), self.to_hex().as_bytes())
    }

    /// Reads `dir/token`, re-running the owner-only check first; a file whose protection was
    /// widened is refused rather than repaired.
    pub fn load(files: &OwnerOnlyFiles<'_>, dir: &std::path::Path) -> Result<Token, TokenError> {
        let bytes = files
            .read(&dir.join(TOKEN_FILE))
            .map_err(TokenError::Guard)?;
        let text = String::from_utf8(bytes).map_err(|_| TokenError::Malformed)?;
        Token::parse_hex(&text)
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

#[derive(Debug)]
pub enum TokenError {
    NoEntropy,
    Malformed,
    Guard(GuardError),
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::NoEntropy => f.write_str("the operating system refused random bytes"),
            TokenError::Malformed => {
                write!(f, "a daemon token is {} hex characters", TOKEN_BYTES * 2)
            }
            TokenError::Guard(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TokenError {}

/// Why a request was refused before routing. No variant says the token was present but wrong,
/// which would confirm a guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthError {
    MissingCredential,
    NotBearer,
    BadToken,
    /// No `Host` header. HTTP/1.1 requires one, so its absence is a hand-written request.
    MissingHost,
    BadHost(String),
    BadOrigin(String),
}

impl AuthError {
    /// 401 for a credential problem, 403 for a `Host` or `Origin` one: a rebinding or cross-site
    /// request is not fixed by better credentials.
    pub fn status(&self) -> u16 {
        match self {
            AuthError::MissingCredential | AuthError::NotBearer | AuthError::BadToken => 401,
            AuthError::MissingHost | AuthError::BadHost(_) | AuthError::BadOrigin(_) => 403,
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::MissingCredential => f.write_str(
                "every request needs `Authorization: Bearer <token>` with the token from the \
                 runtime directory",
            ),
            AuthError::NotBearer => f.write_str("the `Authorization` scheme must be `Bearer`"),
            AuthError::BadToken => f.write_str("not authorized"),
            AuthError::MissingHost => f.write_str("a request needs a `Host` header"),
            AuthError::BadHost(host) => write!(
                f,
                "`Host: {host}` is not this daemon's address; it must be 127.0.0.1 or localhost \
                 with the bound port"
            ),
            AuthError::BadOrigin(origin) => write!(
                f,
                "`Origin: {origin}` is not the served origin, and no CORS is offered"
            ),
        }
    }
}

impl std::error::Error for AuthError {}

/// The three checks, bound to the port the daemon actually listens on (after any fallback to
/// port 0), or a daemon on a fallback port would refuse its own clients.
#[derive(Debug)]
pub struct Auth {
    token: Token,
    port: u16,
}

impl Auth {
    pub fn new(token: Token, port: u16) -> Auth {
        Auth { token, port }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn authorities(&self) -> [String; 2] {
        [
            format!("127.0.0.1:{}", self.port),
            format!("localhost:{}", self.port),
        ]
    }

    pub fn check_bearer(&self, header: Option<&str>) -> Result<(), AuthError> {
        let header = header.ok_or(AuthError::MissingCredential)?;
        let (scheme, value) = header.split_once(' ').ok_or(AuthError::NotBearer)?;
        if !scheme.eq_ignore_ascii_case(BEARER) {
            return Err(AuthError::NotBearer);
        }
        let offered = Token::parse_hex(value.trim()).map_err(|_| AuthError::BadToken)?;
        if self.token.matches(&offered) {
            Ok(())
        } else {
            Err(AuthError::BadToken)
        }
    }

    /// Checks a `Host` header value, case-insensitively. No other name is accepted, so a DNS name
    /// that resolves to 127.0.0.1 cannot reach the daemon from a page.
    pub fn check_host(&self, header: Option<&str>) -> Result<(), AuthError> {
        let host = header.ok_or(AuthError::MissingHost)?.trim();
        if self
            .authorities()
            .iter()
            .any(|a| a.eq_ignore_ascii_case(host))
        {
            Ok(())
        } else {
            Err(AuthError::BadHost(host.to_string()))
        }
    }

    /// Checks an `Origin` header value. An absent one is accepted (CLI, MCP clients and `curl`
    /// send none; the token authorizes them); a present one must be the served origin.
    pub fn check_origin(&self, header: Option<&str>) -> Result<(), AuthError> {
        let Some(origin) = header else {
            return Ok(());
        };
        let origin = origin.trim();
        if self
            .authorities()
            .iter()
            .any(|a| origin.eq_ignore_ascii_case(&format!("http://{a}")))
        {
            Ok(())
        } else {
            Err(AuthError::BadOrigin(origin.to_string()))
        }
    }

    /// The full gate: `Host`, then `Origin`, then the credential, so a cross-site or rebinding
    /// request is refused before the token is looked at and cannot probe it by timing.
    pub fn check(
        &self,
        host: Option<&str>,
        origin: Option<&str>,
        authorization: Option<&str>,
    ) -> Result<(), AuthError> {
        self.check_host(host)?;
        self.check_origin(origin)?;
        self.check_bearer(authorization)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> Token {
        Token::from_bytes([0x11; TOKEN_BYTES])
    }

    fn auth() -> Auth {
        Auth::new(token(), 8765)
    }

    #[test]
    fn hex_round_trips_and_refuses_a_truncated_file() {
        let t = Token::from_bytes([0xab; TOKEN_BYTES]);
        let hex = t.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        assert!(Token::parse_hex(&hex).unwrap().matches(&t));
        assert!(matches!(
            Token::parse_hex(&hex[..62]),
            Err(TokenError::Malformed)
        ));
        assert!(matches!(
            Token::parse_hex(&"z".repeat(64)),
            Err(TokenError::Malformed)
        ));
    }

    #[test]
    fn a_generated_token_is_not_the_previous_one() {
        let a = Token::generate().expect("the OS has entropy");
        let b = Token::generate().expect("the OS has entropy");
        assert!(!a.matches(&b), "two generated tokens must differ");
        assert!(a.matches(&a.clone()));
    }

    #[test]
    fn debug_prints_no_byte_of_the_token() {
        let t = Token::from_bytes([0x7f; TOKEN_BYTES]);
        let printed = format!("{t:?}");
        assert_eq!(printed, "Token(<redacted>)");
        assert!(!printed.contains("7f"));
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive_and_the_token_is_not_guessable() {
        let auth = auth();
        let hex = token().to_hex();
        assert_eq!(auth.check_bearer(Some(&format!("Bearer {hex}"))), Ok(()));
        assert_eq!(auth.check_bearer(Some(&format!("bearer {hex}"))), Ok(()));
        assert_eq!(auth.check_bearer(Some(&format!("BEARER {hex}"))), Ok(()));
        assert_eq!(
            auth.check_bearer(Some(&format!("Basic {hex}"))),
            Err(AuthError::NotBearer)
        );
        assert_eq!(auth.check_bearer(None), Err(AuthError::MissingCredential));
        assert_eq!(auth.check_bearer(Some("Bearer")), Err(AuthError::NotBearer));
        let mut wrong = [0x11; TOKEN_BYTES];
        wrong[TOKEN_BYTES - 1] = 0x12;
        assert_eq!(
            auth.check_bearer(Some(&format!(
                "Bearer {}",
                Token::from_bytes(wrong).to_hex()
            ))),
            Err(AuthError::BadToken)
        );
    }

    #[test]
    fn host_accepts_only_the_two_loopback_authorities_on_the_bound_port() {
        let auth = auth();
        assert_eq!(auth.check_host(Some("127.0.0.1:8765")), Ok(()));
        assert_eq!(auth.check_host(Some("localhost:8765")), Ok(()));
        assert_eq!(auth.check_host(Some("LocalHost:8765")), Ok(()));
        // A rebinding name that resolves to loopback, the attack this check exists for.
        assert!(matches!(
            auth.check_host(Some("evil.example:8765")),
            Err(AuthError::BadHost(_))
        ));
        assert!(matches!(
            auth.check_host(Some("127.0.0.1:8766")),
            Err(AuthError::BadHost(_))
        ));
        assert!(matches!(
            auth.check_host(Some("127.0.0.1")),
            Err(AuthError::BadHost(_))
        ));
        assert_eq!(auth.check_host(None), Err(AuthError::MissingHost));
    }

    #[test]
    fn a_fallback_port_moves_both_checks_with_it() {
        let auth = Auth::new(token(), 49152);
        assert_eq!(auth.check_host(Some("127.0.0.1:49152")), Ok(()));
        assert_eq!(
            auth.check_origin(Some("http://localhost:49152")),
            Ok(()),
            "the checks follow the real port, not the default"
        );
        assert!(matches!(
            auth.check_host(Some("127.0.0.1:8765")),
            Err(AuthError::BadHost(_))
        ));
    }

    #[test]
    fn origin_is_optional_but_a_foreign_one_is_refused() {
        let auth = auth();
        assert_eq!(auth.check_origin(None), Ok(()), "a CLI sends no Origin");
        assert_eq!(auth.check_origin(Some("http://127.0.0.1:8765")), Ok(()));
        assert_eq!(auth.check_origin(Some("http://localhost:8765")), Ok(()));
        for foreign in [
            "http://evil.example",
            "https://127.0.0.1:8765",
            "null",
            "http://127.0.0.1:8765.evil.example",
        ] {
            assert!(
                matches!(
                    auth.check_origin(Some(foreign)),
                    Err(AuthError::BadOrigin(_))
                ),
                "{foreign} must be refused"
            );
        }
    }

    #[test]
    fn host_and_origin_are_checked_before_the_credential() {
        let auth = auth();
        let hex = token().to_hex();
        assert!(matches!(
            auth.check(
                Some("127.0.0.1:8765"),
                Some("http://evil.example"),
                Some(&format!("Bearer {hex}"))
            ),
            Err(AuthError::BadOrigin(_))
        ));
        assert!(matches!(
            auth.check(Some("evil.example:8765"), None, None),
            Err(AuthError::BadHost(_))
        ));
        assert_eq!(
            auth.check(
                Some("127.0.0.1:8765"),
                Some("http://127.0.0.1:8765"),
                Some(&format!("Bearer {hex}"))
            ),
            Ok(())
        );
    }

    #[test]
    fn statuses_separate_a_credential_problem_from_a_site_problem() {
        assert_eq!(AuthError::MissingCredential.status(), 401);
        assert_eq!(AuthError::BadToken.status(), 401);
        assert_eq!(AuthError::NotBearer.status(), 401);
        assert_eq!(AuthError::MissingHost.status(), 403);
        assert_eq!(AuthError::BadHost("x".into()).status(), 403);
        assert_eq!(AuthError::BadOrigin("x".into()).status(), 403);
    }

    #[test]
    fn the_bad_token_message_says_nothing_about_the_token() {
        assert_eq!(AuthError::BadToken.to_string(), "not authorized");
    }
}
