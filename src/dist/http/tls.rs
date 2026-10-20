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

//! The build server's certificate, and the TLS it serves with.

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

use crate::errors::*;

/// A self-signed certificate for this server's address, and its private key.
///
/// What the scheduler and clients verify against: they take this certificate
/// as the only root for this server, through rustls. So it carries the
/// address as an IP subject alternative name, the server-auth key usage and
/// a SHA-256 signature -- rustls refuses SHA-1, which this used to sign
/// with -- and is otherwise as before: RSA 2048, valid for a year, the
/// address as the subject's common name. The serial is derived from the
/// public key rather than fixed at 0, which RFC 5280 does not allow.
///
/// rcgen rather than openssl, for one TLS stack on both ends of a dist
/// connection. The key comes out as PKCS#8, which [`tls_acceptor`] reads.
pub(super) fn create_https_cert_and_privkey(
    addr: SocketAddr,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    use sha2::Digest as _;

    let key = rcgen::KeyPair::generate_rsa_for(&rcgen::PKCS_RSA_SHA256, rcgen::RsaKeySize::_2048)
        .context("failed to generate rsa privkey")?;
    let mut params = rcgen::CertificateParams::default();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, addr.to_string());
    params.subject_alt_names = vec![rcgen::SanType::IpAddress(addr.ip())];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(365);
    let cert = params
        .self_signed(&key)
        .context("failed to sign the server certificate")?;
    // The digest identifies the certificate to the scheduler and clients:
    // SHA-256 over its DER, as openssl's `X509::digest` computed it.
    let cert_digest = sha2::Sha256::digest(cert.der()).to_vec();
    Ok((
        cert_digest,
        cert.pem().into_bytes(),
        key.serialize_pem().into_bytes(),
    ))
}

/// The rustls configuration for the build server's certificate.
///
/// The certificate and key are [`create_https_cert_and_privkey`]'s: rcgen's
/// PEM, the key PKCS#8. The provider is aws-lc-rs, named rather than taken
/// from the process default so that it is the one rustls already builds with
/// here, for the scheduler's and clients' verification, and never a second.
/// TLS 1.2 and 1.3, as rustls offers by default; no ALPN, as tiny_http offered
/// none, so clients speak HTTP/1.1.
pub(super) fn tls_acceptor(cert_pem: &[u8], privkey_pem: &[u8]) -> Result<TlsAcceptor> {
    let cert = CertificateDer::from_pem_slice(cert_pem)
        .map_err(|e| anyhow!("failed to read the server certificate PEM: {e}"))?;
    let key = PrivateKeyDer::from_pem_slice(privkey_pem)
        .map_err(|e| anyhow!("failed to read the server private key PEM: {e}"))?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("failed to choose TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .context("failed to configure TLS with the server certificate")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::{create_https_cert_and_privkey, tls_acceptor};
    use rustls_pki_types::pem::PemObject as _;
    use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
    use sha2::Digest as _;

    /// The certificate is checked the way the scheduler and every client
    /// check it: as the only root for this server, by rustls's verifier, for
    /// the address it was issued for. Each piece has failed in its time --
    /// a SHA-1 signature rustls refused surfaced only as every dist job
    /// being un-assigned, found by the dist tests in a container after
    /// minutes of retries -- and this says so in a unit test instead.
    #[test]
    fn rustls_accepts_the_server_certificate_for_its_address() {
        let (digest, cert_pem, key_pem) =
            create_https_cert_and_privkey("127.0.0.1:10501".parse().unwrap()).unwrap();
        let der = CertificateDer::from_pem_slice(&cert_pem).unwrap();

        let anchor = webpki::anchor_from_trusted_cert(&der).unwrap().to_owned();
        let cert = webpki::EndEntityCert::try_from(&der).unwrap();
        cert.verify_for_usage(
            webpki::ALL_VERIFICATION_ALGS,
            &[anchor],
            &[],
            UnixTime::now(),
            webpki::KeyUsage::server_auth(),
            None,
            None,
        )
        .unwrap();
        cert.verify_is_valid_for_subject_name(&ServerName::try_from("127.0.0.1").unwrap())
            .unwrap();
        // ...and only for that address.
        assert!(
            cert.verify_is_valid_for_subject_name(&ServerName::try_from("127.0.0.2").unwrap())
                .is_err()
        );

        // The digest that identifies it is SHA-256 over the DER.
        assert_eq!(digest, sha2::Sha256::digest(der.as_ref()).to_vec());
        assert!(String::from_utf8_lossy(&key_pem).starts_with("-----BEGIN PRIVATE KEY-----"));
    }

    #[test]
    fn tls_takes_the_rcgen_certificate_and_key() {
        let (_, cert_pem, key_pem) =
            create_https_cert_and_privkey("127.0.0.1:10501".parse().unwrap()).unwrap();
        tls_acceptor(&cert_pem, &key_pem).unwrap();
        // The other way round is not a certificate and a key.
        assert!(tls_acceptor(&key_pem, &cert_pem).is_err());
        // A key that is not the certificate's.
        let (_, _, other_key_pem) =
            create_https_cert_and_privkey("127.0.0.1:10501".parse().unwrap()).unwrap();
        assert!(tls_acceptor(&cert_pem, &other_key_pem).is_err());
    }
}
