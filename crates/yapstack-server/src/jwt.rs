// SPDX-License-Identifier: AGPL-3.0-only
//! JWT access (15 min) + refresh (30 d) with per-device families.
//!
//! `tenant_id` is bound INTO the token at issuance from server-validated state; it is
//! never taken from the request body. Every tenant-scoped handler derives the RLS
//! `app.tenant_id` from the *validated* access token, closing the confused-deputy /
//! IDOR path (architecture §5).

use chrono::Utc;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ACCESS_TTL_SECS: i64 = 15 * 60;
pub const REFRESH_TTL_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessClaims {
    pub sub: Uuid,    // user_id
    pub tenant: Uuid, // workspace_id == tenant_id; bound at issuance
    /// The calling device (§7.1 fresh UUIDv4) this token was minted for. Bound at
    /// login/finish + refresh from server-validated state (the device authenticating
    /// IS the client), NEVER request-supplied — it lets a handler tell WHICH device is
    /// calling (e.g. gate `PUT /devices/roster` to an ACTIVE caller, §7.5). `Option`
    /// because the recovery path logs in without a specific enrolled device yet.
    #[serde(default)]
    pub client_id: Option<Uuid>,
    pub typ: String, // "access"
    pub iat: i64,
    pub exp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshClaims {
    pub sub: Uuid,               // user_id
    pub tenant: Uuid,            // workspace_id
    pub client_id: Option<Uuid>, // device this refresh family belongs to
    pub family: Uuid,            // refresh-token family for reuse detection
    pub jti: Uuid,               // this token's id (rotation)
    pub typ: String,             // "refresh"
    pub iat: i64,
    pub exp: i64,
}

#[derive(Clone)]
pub struct JwtKeys {
    enc: EncodingKey,
    dec: DecodingKey,
}

impl JwtKeys {
    #[must_use]
    pub fn new(secret: &[u8]) -> Self {
        Self {
            enc: EncodingKey::from_secret(secret),
            dec: DecodingKey::from_secret(secret),
        }
    }

    /// Issue a 15-minute access token bound to `(user, tenant, client_id)`. The
    /// `client_id` is the calling device (supplied from server-validated state, never
    /// the request body) so downstream handlers can identify WHICH device is calling.
    ///
    /// # Errors
    /// Propagates a `jsonwebtoken` encoding error (unreachable for HS256).
    pub fn issue_access(
        &self,
        user: Uuid,
        tenant: Uuid,
        client_id: Option<Uuid>,
    ) -> Result<String, jsonwebtoken::errors::Error> {
        let now = Utc::now().timestamp();
        let claims = AccessClaims {
            sub: user,
            tenant,
            client_id,
            typ: "access".to_string(),
            iat: now,
            exp: now + ACCESS_TTL_SECS,
        };
        encode(&Header::default(), &claims, &self.enc)
    }

    /// Issue a 30-day refresh token for a device family. `jti`/`family` are supplied
    /// by the caller so the DB row and the token agree.
    ///
    /// # Errors
    /// Propagates a `jsonwebtoken` encoding error.
    pub fn issue_refresh(
        &self,
        user: Uuid,
        tenant: Uuid,
        client_id: Option<Uuid>,
        family: Uuid,
        jti: Uuid,
    ) -> Result<String, jsonwebtoken::errors::Error> {
        let now = Utc::now().timestamp();
        let claims = RefreshClaims {
            sub: user,
            tenant,
            client_id,
            family,
            jti,
            typ: "refresh".to_string(),
            iat: now,
            exp: now + REFRESH_TTL_SECS,
        };
        encode(&Header::default(), &claims, &self.enc)
    }

    /// Validate an access token (signature, expiry, and `typ == "access"`).
    ///
    /// # Errors
    /// Returns a `jsonwebtoken` error on any validation failure.
    pub fn verify_access(&self, token: &str) -> Result<AccessClaims, jsonwebtoken::errors::Error> {
        let data = decode::<AccessClaims>(token, &self.dec, &Validation::default())?;
        if data.claims.typ != "access" {
            return Err(jsonwebtoken::errors::ErrorKind::InvalidToken.into());
        }
        Ok(data.claims)
    }

    /// Validate a refresh token (signature, expiry, and `typ == "refresh"`).
    ///
    /// # Errors
    /// Returns a `jsonwebtoken` error on any validation failure.
    pub fn verify_refresh(
        &self,
        token: &str,
    ) -> Result<RefreshClaims, jsonwebtoken::errors::Error> {
        let data = decode::<RefreshClaims>(token, &self.dec, &Validation::default())?;
        if data.claims.typ != "refresh" {
            return Err(jsonwebtoken::errors::ErrorKind::InvalidToken.into());
        }
        Ok(data.claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_token_roundtrips_and_binds_tenant() {
        let keys = JwtKeys::new(b"test-secret");
        let user = Uuid::new_v4();
        let tenant = Uuid::new_v4();
        let client = Uuid::new_v4();
        let token = keys.issue_access(user, tenant, Some(client)).unwrap();
        let claims = keys.verify_access(&token).unwrap();
        assert_eq!(claims.sub, user);
        assert_eq!(claims.tenant, tenant);
        assert_eq!(claims.client_id, Some(client));
        assert_eq!(claims.typ, "access");
    }

    #[test]
    fn access_verifier_rejects_refresh_token() {
        let keys = JwtKeys::new(b"test-secret");
        let (u, t, f, j) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let refresh = keys.issue_refresh(u, t, None, f, j).unwrap();
        // A refresh token MUST NOT validate as an access token.
        assert!(keys.verify_access(&refresh).is_err());
        // And vice versa.
        let access = keys.issue_access(u, t, None).unwrap();
        assert!(keys.verify_refresh(&access).is_err());
    }

    #[test]
    fn wrong_secret_rejected() {
        let keys = JwtKeys::new(b"secret-a");
        let other = JwtKeys::new(b"secret-b");
        let token = keys
            .issue_access(Uuid::new_v4(), Uuid::new_v4(), None)
            .unwrap();
        assert!(other.verify_access(&token).is_err());
    }
}
