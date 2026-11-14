//! Local accounts: password hashing, session tokens and the session cookie
//! (future.md Phase 3, milestone M1).
//!
//! Accounts are created by the operator (`hunter user add`); the HTTP side
//! only logs in and out. A session is a random token in an `HttpOnly`,
//! `SameSite=Strict` cookie; the database keeps only its SHA-256, so a copy
//! of the database logs nobody in.

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::http::{HeaderMap, HeaderValue, header};
use sha2::{Digest, Sha256};

use crate::store::Store;

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "hunter_session";

/// How long a login lasts, in milliseconds: 30 days. There is no sliding
/// renewal; a session past this logs in again.
pub const SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Shortest password `hunter user` accepts. The dashboard can be exposed
/// beyond loopback (`serve.host`), and every login attempt costs the
/// attacker only one argon2 evaluation.
pub const MIN_PASSWORD_CHARS: usize = 12;

/// The account a request was authenticated as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
}

/// Whether `name` may be a username: 1-64 characters of ASCII letters,
/// digits, `.`, `_` and `-`. Kept narrow so a name prints the same in a
/// log line, an event message and a terminal.
pub fn validate_username(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err("a username must be 1 to 64 characters".to_owned());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "username {name:?} may only contain ASCII letters, digits, '.', '_' and '-'"
        ));
    }
    Ok(())
}

/// Whether `password` is long enough ([`MIN_PASSWORD_CHARS`]).
pub fn validate_password(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(format!(
            "a password must be at least {MIN_PASSWORD_CHARS} characters"
        ));
    }
    Ok(())
}

/// argon2id PHC string for `password`, with a fresh random salt.
///
/// Deliberately slow (tens of milliseconds); call it off the async
/// runtime.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let hash = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))?;
    Ok(hash.to_string())
}

/// Whether `password` matches the PHC string `hash`. A malformed hash
/// matches nothing.
///
/// As slow as [`hash_password`]; call it off the async runtime.
pub fn verify_password(password: &str, hash: &str) -> bool {
    Argon2::default()
        .verify_password(password.as_bytes(), hash)
        .is_ok()
}

/// A hash no password is known for. Login verifies against it when the
/// username does not exist, so an unknown name costs as long as a wrong
/// password and response time does not tell which names are accounts.
///
/// Precomputed (from 32 random bytes, then discarded) rather than hashed
/// at startup: a runtime hash could fail, and a failure fallback would be
/// a malformed hash that skips argon2 -- exactly the fast path this exists
/// to remove. Its parameters must stay those [`hash_password`] uses, which
/// `the_decoy_costs_what_a_real_hash_costs` checks.
pub const DECOY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$8ctNPuHuM+GaQyKfs/iUDw$j+UIhOp01a87bkTUl0syGOfnWfbPi9y2Pgc8qx/LjT4";

/// A new session token: 32 random bytes, hex-encoded.
pub fn new_session_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness for a session: {e}"))?;
    Ok(hex(&bytes))
}

/// What the database stores for a session token.
pub fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// The session token from a request's `Cookie` headers, if any.
pub fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == SESSION_COOKIE && !value.is_empty()).then_some(value)
        })
}

/// `Set-Cookie` value that stores `token` for [`SESSION_TTL_MS`].
///
/// `HttpOnly`: page scripts never see it. `SameSite=Strict`: no other site
/// can make the browser send it, which together with the JSON
/// Content-Type gate on every POST rules out cross-site requests. No
pub fn session_cookie(token: &str) -> anyhow::Result<HeaderValue> {
    let max_age_s = SESSION_TTL_MS / 1000;
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_s}"
    ))
    .map_err(|e| anyhow::anyhow!("session cookie: {e}"))
}

/// `Set-Cookie` value that deletes the session cookie.
pub fn cleared_session_cookie() -> HeaderValue {
    HeaderValue::from_static("hunter_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// `bytes` as lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

// -- account administration (`hunter user ...`) -------------------------------

/// Create account `username` with `password`, both validated first.
pub async fn add_user(store: &Store, username: &str, password: String) -> anyhow::Result<i64> {
    validate_username(username).map_err(anyhow::Error::msg)?;
    validate_password(&password).map_err(anyhow::Error::msg)?;
    let hash = hash_off_runtime(password).await?;
    Ok(store.create_user(username, &hash).await?)
}

/// Give `username` a new password, logging out its sessions.
pub async fn change_password(
    store: &Store,
    username: &str,
    password: String,
) -> anyhow::Result<()> {
    validate_password(&password).map_err(anyhow::Error::msg)?;
    let hash = hash_off_runtime(password).await?;
    anyhow::ensure!(
        store.set_password(username, &hash).await?,
        "no user {username:?}"
    );
    Ok(())
}

/// Disable `username` and log out its sessions.
pub async fn disable_user(store: &Store, username: &str) -> anyhow::Result<()> {
    anyhow::ensure!(store.disable_user(username).await?, "no user {username:?}");
    Ok(())
}

async fn hash_off_runtime(password: String) -> anyhow::Result<String> {
    crate::child::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))?
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cookies(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for v in values {
            headers.append(header::COOKIE, HeaderValue::from_str(v).unwrap());
        }
        headers
    }

    /// The token is found among other cookies and across `Cookie`
    /// headers; a cookie that merely ends in the name, or an empty value,
    /// is not a session.
    #[test]
    fn the_session_token_is_read_from_among_other_cookies() {
        assert_eq!(
            session_token(&cookies(&["theme=dark; hunter_session=abc; lang=de"])),
            Some("abc")
        );
        assert_eq!(
            session_token(&cookies(&["theme=dark", "hunter_session=abc"])),
            Some("abc")
        );
        assert_eq!(session_token(&cookies(&["xhunter_session=abc"])), None);
        assert_eq!(session_token(&cookies(&["hunter_session="])), None);
        assert_eq!(session_token(&HeaderMap::new()), None);
    }

    /// Usernames: 1 to 64 characters from a small alphabet.
    #[test]
    fn usernames_are_short_and_plain() {
        assert!(validate_username("a").is_ok());
        assert!(validate_username(&"a".repeat(64)).is_ok());
        assert!(validate_username(&"a".repeat(65)).is_err());
        assert!(validate_username("").is_err());
        assert!(validate_username("dan.f-r_1").is_ok());
        assert!(validate_username("dan/f").is_err());
        assert!(validate_username("dän").is_err());
    }

    /// Passwords: at least [`MIN_PASSWORD_CHARS`] characters, counted as
    /// characters rather than bytes.
    #[test]
    fn passwords_need_twelve_characters() {
        assert!(validate_password(&"x".repeat(11)).is_err());
        assert!(validate_password(&"x".repeat(12)).is_ok());
        assert!(
            validate_password(&"ä".repeat(11)).is_err(),
            "22 bytes, 11 characters"
        );
    }

    /// A hash verifies its own password and nothing else; tokens are
    /// unique 64-hex-digit strings stored only as their 32-byte digest.
    #[test]
    fn hashes_and_tokens_behave() {
        let hash = hash_password("correct horse battery").unwrap();
        assert!(verify_password("correct horse battery", &hash));
        assert!(!verify_password("correct horse batterY", &hash));
        assert!(!verify_password("correct horse battery", "not a hash"));

        let a = new_session_token().unwrap();
        let b = new_session_token().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(token_hash(&a).len(), 32);
        assert_ne!(token_hash(&a), token_hash(&b));
    }

    /// The decoy an unknown username is checked against must cost what a
    /// real account's hash costs -- same algorithm, version and
    /// parameters -- or its speed tells an attacker the name does not
    /// exist. Fails if the argon2 defaults ever move away from it.
    #[test]
    fn the_decoy_costs_what_a_real_hash_costs() {
        let params = |phc: &str| phc.rsplitn(3, '$').nth(2).unwrap().to_owned();
        let real = hash_password("anything at all").unwrap();
        assert_eq!(params(DECOY_HASH), params(&real));
        assert!(!verify_password("", DECOY_HASH));
    }
}
