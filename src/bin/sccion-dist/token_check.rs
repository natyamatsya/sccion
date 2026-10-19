// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result, bail};
use sccion::dist::http::{ClientAuthCheck, ClientVisibleMsg};
use sccion::util::new_reqwest_blocking_client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::result::Result as StdResult;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// https://auth0.com/docs/jwks
#[derive(Debug, Serialize, Deserialize)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Jwk {
    pub kid: String,
    kty: String,
    n: String,
    e: String,
}

impl Jwk {
    /// The key this JWK describes, ready to verify a signature with.
    ///
    /// `n` and `e` go to jsonwebtoken as they are -- base64url without
    /// padding, which is how a JWK spells them and how it decodes them -- so
    /// there is no detour through a DER-encoded public key, and no openssl.
    pub fn decoding_key(&self) -> Result<jwt::DecodingKey> {
        if self.kty != "RSA" {
            bail!("Cannot handle non-RSA JWK")
        }
        jwt::DecodingKey::from_rsa_components(&self.n, &self.e)
            .context("Failed to read the RSA modulus and exponent")
    }
}

// Check a token is equal to a fixed string
pub struct EqCheck {
    s: String,
}

impl ClientAuthCheck for EqCheck {
    fn check(&self, token: &str) -> StdResult<(), ClientVisibleMsg> {
        if self.s == token {
            Ok(())
        } else {
            warn!("User token {} != expected token {}", token, self.s);
            Err(ClientVisibleMsg::from_nonsensitive(
                "Fixed token mismatch".to_owned(),
            ))
        }
    }
}

impl EqCheck {
    pub fn new(s: String) -> Self {
        Self { s }
    }
}

// Don't check a token is valid (it may not even be a JWT) just forward it to
// an API and check for success
pub struct ProxyTokenCheck {
    client: reqwest::blocking::Client,
    maybe_auth_cache: Option<Mutex<(HashMap<String, Instant>, Duration)>>,
    url: String,
}

impl ClientAuthCheck for ProxyTokenCheck {
    fn check(&self, token: &str) -> StdResult<(), ClientVisibleMsg> {
        match self.check_token_with_forwarding(token) {
            Ok(()) => Ok(()),
            Err(e) => {
                warn!("Proxying token validation failed: {}", e);
                Err(ClientVisibleMsg::from_nonsensitive(
                    "Validation with token forwarding failed".to_owned(),
                ))
            }
        }
    }
}

impl ProxyTokenCheck {
    pub fn new(url: String, cache_secs: Option<u64>) -> Self {
        let maybe_auth_cache: Option<Mutex<(HashMap<String, Instant>, Duration)>> =
            cache_secs.map(|secs| Mutex::new((HashMap::new(), Duration::from_secs(secs))));
        Self {
            client: new_reqwest_blocking_client(),
            maybe_auth_cache,
            url,
        }
    }

    fn check_token_with_forwarding(&self, token: &str) -> Result<()> {
        trace!("Validating token by forwarding to {}", self.url);
        // If the token is cached and not cache has not expired, return it
        if let Some(ref auth_cache) = self.maybe_auth_cache {
            let mut auth_cache = auth_cache.lock().unwrap();
            let (ref mut auth_cache, cache_duration) = *auth_cache;
            if let Some(cached_at) = auth_cache.get(token)
                && cached_at.elapsed() < cache_duration
            {
                return Ok(());
            }
            auth_cache.remove(token);
        }
        // Make a request to another API, which as a side effect should actually check the token
        let res = self
            .client
            .get(&self.url)
            .bearer_auth(token)
            .send()
            .context("Failed to make request to proxying url")?;
        if !res.status().is_success() {
            bail!("Token forwarded to {} returned {}", self.url, res.status());
        }
        // Cache the token
        if let Some(ref auth_cache) = self.maybe_auth_cache {
            let mut auth_cache = auth_cache.lock().unwrap();
            let (ref mut auth_cache, _) = *auth_cache;
            auth_cache.insert(token.to_owned(), Instant::now());
        }
        Ok(())
    }
}

// Check a JWT is valid
pub struct ValidJWTCheck {
    audience: String,
    issuer: String,
    kid_to_key: HashMap<String, jwt::DecodingKey>,
}

impl ClientAuthCheck for ValidJWTCheck {
    fn check(&self, token: &str) -> StdResult<(), ClientVisibleMsg> {
        match self.check_jwt_validity(token) {
            Ok(()) => Ok(()),
            Err(e) => {
                warn!("JWT validation failed: {}", e);
                Err(ClientVisibleMsg::from_nonsensitive(
                    "JWT could not be validated".to_owned(),
                ))
            }
        }
    }
}

impl ValidJWTCheck {
    pub fn new(audience: String, issuer: String, jwks_url: &str) -> Result<Self> {
        let res = reqwest::blocking::get(jwks_url).context("Failed to make request to JWKs url")?;
        if !res.status().is_success() {
            bail!("Could not retrieve JWKs, HTTP error: {}", res.status())
        }
        let jwks: Jwks = res.json().context("Failed to parse JWKs json")?;
        let kid_to_key = jwks
            .keys
            .into_iter()
            .map(|k| k.decoding_key().map(|key| (k.kid, key)))
            .collect::<Result<_>>()
            .context("Failed to read the JWKs")?;
        Ok(Self {
            audience,
            issuer,
            kid_to_key,
        })
    }

    fn check_jwt_validity(&self, token: &str) -> Result<()> {
        let header = jwt::decode_header(token).context("Could not decode jwt header")?;
        trace!("Validating JWT in scheduler");
        // Prepare validation
        let kid = header.kid.context("No kid found")?;
        let key = self.kid_to_key.get(&kid).context("kid not found in jwks")?;
        let mut validation = jwt::Validation::new(header.alg);
        validation.set_audience(&[&self.audience]);
        validation.set_issuer(&[&self.issuer]);
        #[derive(Deserialize)]
        struct Claims {}
        // Decode the JWT, discarding any claims - we just care about validity
        let _tokendata = jwt::decode::<Claims>(token, key, &validation)
            .context("Unable to validate and decode jwt")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Jwk, ValidJWTCheck};
    use base64::Engine;
    use sccion::util::BASE64_URL_SAFE_ENGINE;
    use serde::Serialize;
    use std::collections::HashMap;

    /// `(n, e)` from a PKCS#1 RSAPublicKey -- SEQUENCE { INTEGER n, INTEGER e }
    /// -- which is how rcgen hands out an RSA key's public half.
    fn rsa_components(der: &[u8]) -> (Vec<u8>, Vec<u8>) {
        fn tlv(b: &[u8]) -> (u8, &[u8], &[u8]) {
            let (len, header) = if b[1] & 0x80 == 0 {
                (usize::from(b[1]), 2)
            } else {
                let n = usize::from(b[1] & 0x7f);
                let len = b[2..2 + n].iter().fold(0, |a, &x| a << 8 | usize::from(x));
                (len, 2 + n)
            };
            (b[0], &b[header..header + len], &b[header + len..])
        }
        let (sequence, body, _) = tlv(der);
        let (int_n, n, rest) = tlv(body);
        let (int_e, e, _) = tlv(rest);
        assert_eq!((sequence, int_n, int_e), (0x30, 0x02, 0x02));
        // DER pads a positive integer with a leading zero byte; a JWK does not.
        let unpad = |v: &[u8]| {
            v.iter()
                .skip_while(|&&b| b == 0)
                .copied()
                .collect::<Vec<_>>()
        };
        (unpad(n), unpad(e))
    }

    #[derive(Serialize)]
    struct Claims<'a> {
        aud: &'a str,
        iss: &'a str,
        exp: u64,
    }

    /// A token signed by a key is accepted through that key's JWK, and nothing
    /// else is: not another key, not another audience or issuer. The JWK is
    /// built from the key's modulus and exponent the way an identity provider
    /// publishes them -- which is the conversion that used to go through
    /// openssl, and is now jsonwebtoken's own.
    #[test]
    fn a_jwk_verifies_tokens_signed_by_its_key_and_no_others() {
        let jwk_for = |key: &rcgen::KeyPair, kid: &str| {
            let (n, e) = rsa_components(key.public_key_raw());
            Jwk {
                kid: kid.to_owned(),
                kty: "RSA".to_owned(),
                n: BASE64_URL_SAFE_ENGINE.encode(n),
                e: BASE64_URL_SAFE_ENGINE.encode(e),
            }
        };
        let generate = || {
            rcgen::KeyPair::generate_rsa_for(&rcgen::PKCS_RSA_SHA256, rcgen::RsaKeySize::_2048)
                .unwrap()
        };
        let (signer, other) = (generate(), generate());
        let check = ValidJWTCheck {
            audience: "sccion".to_owned(),
            issuer: "https://issuer.example".to_owned(),
            kid_to_key: HashMap::from([
                (
                    "signer".to_owned(),
                    jwk_for(&signer, "signer").decoding_key().unwrap(),
                ),
                (
                    "other".to_owned(),
                    jwk_for(&other, "other").decoding_key().unwrap(),
                ),
            ]),
        };

        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600;
        let sign = |kid: &str, aud: &str, iss: &str| {
            let mut header = jwt::Header::new(jwt::Algorithm::RS256);
            header.kid = Some(kid.to_owned());
            let key = jwt::EncodingKey::from_rsa_pem(signer.serialize_pem().as_bytes()).unwrap();
            jwt::encode(&header, &Claims { aud, iss, exp }, &key).unwrap()
        };

        let good = sign("signer", "sccion", "https://issuer.example");
        assert!(
            check.check_jwt_validity(&good).is_ok(),
            "{:?}",
            check.check_jwt_validity(&good)
        );
        // Signed by `signer` but claiming to be `other`'s: the signature fails.
        assert!(
            check
                .check_jwt_validity(&sign("other", "sccion", "https://issuer.example"))
                .is_err()
        );
        assert!(
            check
                .check_jwt_validity(&sign("signer", "elsewhere", "https://issuer.example"))
                .is_err()
        );
        assert!(
            check
                .check_jwt_validity(&sign("signer", "sccion", "https://evil.example"))
                .is_err()
        );
        assert!(
            check
                .check_jwt_validity(&sign("unknown", "sccion", "https://issuer.example"))
                .is_err()
        );
    }

    #[test]
    fn a_non_rsa_jwk_is_refused() {
        let jwk = Jwk {
            kid: "k".to_owned(),
            kty: "EC".to_owned(),
            n: String::new(),
            e: String::new(),
        };
        assert!(jwk.decoding_key().is_err());
    }
}
