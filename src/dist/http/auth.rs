// Copyright 2016 Mozilla Foundation
// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Who may ask what of the scheduler and the build server: client and build
//! server tokens, and the per-job tokens a build server signs.

use axum::http::{HeaderMap, header};
use serde::{Deserialize, Serialize};
use std::result::Result as StdResult;
use std::sync::LazyLock;

use crate::dist::{self, JobId, ServerId};
use crate::errors::*;

// Messages that are non-sensitive and can be sent to the client
#[derive(Debug)]
pub struct ClientVisibleMsg(pub(super) String);
impl ClientVisibleMsg {
    pub fn from_nonsensitive(s: String) -> Self {
        ClientVisibleMsg(s)
    }
}

pub trait ClientAuthCheck: Send + Sync {
    fn check(&self, token: &str) -> StdResult<(), ClientVisibleMsg>;
}
pub type ServerAuthCheck = Box<dyn Fn(&str) -> Option<ServerId> + Send + Sync>;

/// The bearer token in `Authorization`: the scheme must be exactly `Bearer`,
/// and the token is the rest after the first space, which may be empty.
pub(super) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let header = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let mut split = header.splitn(2, ' ');
    if split.next()? != "Bearer" {
        return None;
    }
    split.next()
}

pub(super) const JWT_KEY_LENGTH: usize = 256 / 8;
static JWT_HEADER: LazyLock<jwt::Header> =
    LazyLock::new(|| jwt::Header::new(jwt::Algorithm::HS256));
static JWT_VALIDATION: LazyLock<jwt::Validation> = LazyLock::new(|| {
    let mut validation = jwt::Validation::new(jwt::Algorithm::HS256);
    validation.leeway = 0;
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation
});

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct JobJwt {
    pub exp: u64,
    pub job_id: dist::JobId,
}

// Generation and verification of job auth
pub(super) struct JWTJobAuthorizer {
    server_key: Vec<u8>,
}
impl JWTJobAuthorizer {
    pub(super) fn new(server_key: Vec<u8>) -> Box<Self> {
        Box::new(Self { server_key })
    }
}
impl dist::JobAuthorizer for JWTJobAuthorizer {
    fn generate_token(&self, job_id: JobId) -> Result<String> {
        let claims = JobJwt { exp: 0, job_id };
        let key = jwt::EncodingKey::from_secret(&self.server_key);
        jwt::encode(&JWT_HEADER, &claims, &key)
            .map_err(|e| anyhow!("Failed to create JWT for job: {}", e))
    }
    fn verify_token(&self, job_id: JobId, token: &str) -> Result<()> {
        let valid_claims = JobJwt { exp: 0, job_id };
        let key = jwt::DecodingKey::from_secret(&self.server_key);
        jwt::decode(token, &key, &JWT_VALIDATION)
            .map_err(|e| anyhow!("JWT decode failed: {}", e))
            .and_then(|res| {
                fn identical_t<T>(_: &T, _: &T) {}
                identical_t(&res.claims, &valid_claims);
                if res.claims == valid_claims {
                    Ok(())
                } else {
                    Err(anyhow!("mismatched claims"))
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dist::JobAuthorizer as _;

    #[test]
    fn test_job_token_verification() {
        let ja = JWTJobAuthorizer::new(vec![1, 2, 2]);

        let job_id = JobId(55);
        let token = ja.generate_token(job_id).unwrap();

        let job_id2 = JobId(56);
        let token2 = ja.generate_token(job_id2).unwrap();

        let ja2 = JWTJobAuthorizer::new(vec![1, 2, 3]);

        // Check tokens are deterministic
        assert_eq!(token, ja.generate_token(job_id).unwrap());
        // Check token verification works
        assert!(ja.verify_token(job_id, &token).is_ok());
        assert!(ja.verify_token(job_id, &token2).is_err());
        assert!(ja.verify_token(job_id2, &token).is_err());
        assert!(ja.verify_token(job_id2, &token2).is_ok());
        // Check token verification with a different key fails
        assert!(ja2.verify_token(job_id, &token).is_err());
        assert!(ja2.verify_token(job_id2, &token2).is_err());
    }

    #[test]
    fn bearer_token_is_read_as_the_protocol_has_it() {
        let headers = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            headers
        };
        assert_eq!(bearer_token(&headers("Bearer abc def")), Some("abc def"));
        assert_eq!(bearer_token(&headers("Bearer ")), Some(""));
        assert_eq!(bearer_token(&headers("Bearer")), None);
        assert_eq!(bearer_token(&headers("bearer abc")), None);
        assert_eq!(bearer_token(&headers("Basic abc")), None);
        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }
}
