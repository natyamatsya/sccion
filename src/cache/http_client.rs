// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

use opendal::{HttpTransporter, OperationContext};
use opendal_http_transport_reqwest::ReqwestTransport;
use reqwest::ClientBuilder;

/// Set the user agent (helps with monitoring on the server side)
///
/// Carried on the operator's context rather than a layer: opendal 0.58
/// replaced `HttpClientLayer` with a transport on `OperationContext`, which
/// is the same client reaching the same requests by another route.
pub fn set_user_agent() -> OperationContext {
    let user_agent = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    let client = ClientBuilder::new()
        .user_agent(user_agent)
        .tls_certs_merge(crate::util::bundled_root_certs())
        .build()
        .unwrap();
    OperationContext::new().with_http_transport(HttpTransporter::new(ReqwestTransport::new(client)))
}
