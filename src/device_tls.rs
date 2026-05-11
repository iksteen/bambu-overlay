//! TLS policy shared by Bambu device-local services.
//!
//! Video uses this today. A future local MQTT transport should use the same
//! connector and device-ID check rather than duplicating certificate handling.

use anyhow::{Context, Result};
use native_tls::{Certificate, TlsConnector as NativeTlsConnector};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_native_tls::{TlsConnector, TlsStream};
use x509_parser::{certificate::X509Certificate, prelude::FromDer};

const BBL_CA_CERT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIDZTCCAk2gAwIBAgIUV1FckwXElyek1onFnQ9kL7Bk4N8wDQYJKoZIhvcNAQEL
BQAwQjELMAkGA1UEBhMCQ04xIjAgBgNVBAoMGUJCTCBUZWNobm9sb2dpZXMgQ28u
LCBMdGQxDzANBgNVBAMMBkJCTCBDQTAeFw0yMjA0MDQwMzQyMTFaFw0zMjA0MDEw
MzQyMTFaMEIxCzAJBgNVBAYTAkNOMSIwIAYDVQQKDBlCQkwgVGVjaG5vbG9naWVz
IENvLiwgTHRkMQ8wDQYDVQQDDAZCQkwgQ0EwggEiMA0GCSqGSIb3DQEBAQUAA4IB
DwAwggEKAoIBAQDL3pnDdxGOk5Z6vugiT4dpM0ju+3Xatxz09UY7mbj4tkIdby4H
oeEdiYSZjc5LJngJuCHwtEbBJt1BriRdSVrF6M9D2UaBDyamEo0dxwSaVxZiDVWC
eeCPdELpFZdEhSNTaT4O7zgvcnFsfHMa/0vMAkvE7i0qp3mjEzYLfz60axcDoJLk
p7n6xKXI+cJbA4IlToFjpSldPmC+ynOo7YAOsXt7AYKY6Glz0BwUVzSJxU+/+VFy
/QrmYGNwlrQtdREHeRi0SNK32x1+bOndfJP0sojuIrDjKsdCLye5CSZIvqnbowwW
1jRwZgTBR29Zp2nzCoxJYcU9TSQp/4KZuWNVAgMBAAGjUzBRMB0GA1UdDgQWBBSP
NEJo3GdOj8QinsV8SeWr3US+HjAfBgNVHSMEGDAWgBSPNEJo3GdOj8QinsV8SeWr
3US+HjAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQABlBIT5ZeG
fgcK1LOh1CN9sTzxMCLbtTPFF1NGGA13mApu6j1h5YELbSKcUqfXzMnVeAb06Htu
3CoCoe+wj7LONTFO++vBm2/if6Jt/DUw1CAEcNyqeh6ES0NX8LJRVSe0qdTxPJuA
BdOoo96iX89rRPoxeed1cpq5hZwbeka3+CJGV76itWp35Up5rmmUqrlyQOr/Wax6
itosIzG0MfhgUzU51A2P/hSnD3NDMXv+wUY/AvqgIL7u7fbDKnku1GzEKIkfH8hm
Rs6d8SCU89xyrwzQ0PR853irHas3WrHVqab3P+qNwR0YirL0Qk7Xt/q3O1griNg2
Blbjg3obpHo9
-----END CERTIFICATE-----"#;

pub(crate) fn tokio_connector() -> Result<TlsConnector> {
    native_connector().map(TlsConnector::from)
}

pub(crate) fn native_connector() -> Result<NativeTlsConnector> {
    let ca = Certificate::from_pem(BBL_CA_CERT_PEM.as_bytes())
        .context("failed to parse embedded BBL CA certificate")?;
    let mut builder = NativeTlsConnector::builder();
    builder.disable_built_in_roots(true);
    builder.use_sni(true);
    builder.add_root_certificate(ca);
    // Bambu device certificates identify printers by serial number in the subject CN.
    // Platform TLS stacks do not consistently accept CN-only certificates for hostname
    // verification, so native TLS verifies the BBL CA chain and callers check CN.
    builder.danger_accept_invalid_hostnames(true);
    builder
        .build()
        .context("failed to build Bambu device TLS connector")
}

pub(crate) fn peer_device_id<S>(socket: &TlsStream<S>) -> Result<String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let certificate = socket
        .get_ref()
        .peer_certificate()
        .context("failed to read Bambu device certificate")?
        .context("Bambu device did not send a certificate")?;
    certificate_device_id(&certificate)
}

pub(crate) fn certificate_device_id(certificate: &Certificate) -> Result<String> {
    let certificate = certificate
        .to_der()
        .context("failed to export Bambu device certificate")?;
    let certificate = parse_x509_certificate(&certificate).map_err(anyhow::Error::msg)?;
    certificate_common_name(&certificate).context("Bambu device certificate common name is empty")
}

fn parse_x509_certificate(der: &[u8]) -> std::result::Result<X509Certificate<'_>, String> {
    let (remaining, certificate) =
        X509Certificate::from_der(der).map_err(|error| error.to_string())?;
    if !remaining.is_empty() {
        return Err(format!(
            "certificate has {} bytes of trailing data",
            remaining.len()
        ));
    }
    Ok(certificate)
}

fn certificate_common_name(certificate: &X509Certificate<'_>) -> Option<String> {
    certificate
        .subject()
        .iter_common_name()
        .find_map(|common_name| common_name.as_str().ok())
        .map(str::trim)
        .filter(|common_name| !common_name.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use native_tls::Certificate;

    use super::{
        certificate_common_name, native_connector, parse_x509_certificate, BBL_CA_CERT_PEM,
    };

    #[test]
    fn bbl_ca_certificate_parses_for_native_tls() {
        let ca = Certificate::from_pem(BBL_CA_CERT_PEM.as_bytes())
            .expect("native TLS should parse embedded CA");
        let ca_der = ca.to_der().expect("native TLS should export embedded CA");
        let ca = parse_x509_certificate(&ca_der).expect("embedded CA should parse");

        assert_eq!(certificate_common_name(&ca).as_deref(), Some("BBL CA"));
    }

    #[test]
    fn native_connector_builds_with_bbl_ca() {
        native_connector().expect("Bambu device TLS connector should build");
    }
}
