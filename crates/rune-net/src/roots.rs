//! Certificate roots for provider requests.
//!
//! The compiled-in roots cannot be extended without another dependency, so a
//! bundle named by `SSL_CERT_FILE` replaces them with its own certificates
//! plus the system bundle. A bundle that cannot be used leaves the compiled-in
//! roots in place and says why, once, at startup.

use std::sync::Arc;

use ureq::tls::{Certificate, PemItem, RootCerts, TlsConfig};

/// The roots requests trust.
#[derive(Clone, Debug)]
pub enum Roots {
    /// The compiled-in roots, because `SSL_CERT_FILE` is unset.
    BuiltIn,
    /// The certificates of `SSL_CERT_FILE`, then those of a system bundle.
    Extra {
        /// Every certificate trusted, the extra ones first.
        certificates: Vec<Certificate<'static>>,
        /// How many came from `SSL_CERT_FILE`.
        from_file: usize,
        /// The system bundle added, when one was readable.
        system: Option<&'static str>,
    },
    /// `SSL_CERT_FILE` is set and unusable; the compiled-in roots are used.
    Unusable(String),
}

impl Roots {
    /// Resolves the roots from `SSL_CERT_FILE` and the system bundles.
    #[must_use]
    pub fn from_environment() -> Self {
        match rune_core::tls::extra_ca_bundle() {
            None => Self::BuiltIn,
            Some(bundle) => Self::from_bundles(Some(bundle), rune_core::tls::system_bundle()),
        }
    }

    /// Resolves the roots from an extra bundle and a system bundle.
    #[must_use]
    pub fn from_bundles(
        extra: Option<Result<Vec<u8>, String>>,
        system: Option<(&'static str, Vec<u8>)>,
    ) -> Self {
        let bytes = match extra {
            None => return Self::BuiltIn,
            Some(Err(reason)) => {
                return Self::Unusable(format!(
                    "SSL_CERT_FILE could not be read ({reason}); using the built-in certificate roots"
                ));
            }
            Some(Ok(bytes)) => bytes,
        };
        let mut certificates = certificates(&bytes);
        if certificates.is_empty() {
            return Self::Unusable(
                "SSL_CERT_FILE holds no certificates; using the built-in certificate roots"
                    .to_owned(),
            );
        }
        let from_file = certificates.len();
        let system = system.map(|(path, bytes)| {
            certificates.extend(self::certificates(&bytes));
            path
        });
        Self::Extra {
            certificates,
            from_file,
            system,
        }
    }

    /// The startup warning, when the bundle could not be used.
    #[must_use]
    pub fn warning(&self) -> Option<&str> {
        match self {
            Self::Unusable(text) => Some(text),
            _ => None,
        }
    }

    /// A one-line description for `rune doctor`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::BuiltIn => "built-in roots".to_owned(),
            Self::Extra {
                from_file, system, ..
            } => {
                let noun = if *from_file == 1 {
                    "certificate"
                } else {
                    "certificates"
                };
                match system {
                    Some(path) => format!("SSL_CERT_FILE ({from_file} {noun}) plus {path}"),
                    None => format!("SSL_CERT_FILE ({from_file} {noun})"),
                }
            }
            Self::Unusable(text) => text.clone(),
        }
    }

    /// The root setting an agent is built with.
    #[must_use]
    pub fn root_certs(&self) -> RootCerts {
        match self {
            Self::Extra { certificates, .. } => RootCerts::Specific(Arc::new(certificates.clone())),
            Self::BuiltIn | Self::Unusable(_) => RootCerts::WebPki,
        }
    }

    /// The TLS configuration an agent is built with.
    #[must_use]
    pub fn tls_config(&self) -> TlsConfig {
        TlsConfig::builder().root_certs(self.root_certs()).build()
    }
}

/// Every certificate in a PEM bundle, skipping anything else and any bad block.
fn certificates(pem: &[u8]) -> Vec<Certificate<'static>> {
    ureq::tls::parse_pem(pem)
        .filter_map(|item| match item {
            Ok(PemItem::Certificate(certificate)) => Some(certificate),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CA: &[u8] = include_bytes!("../tests/fixtures/tls/ca.pem");

    #[test]
    fn a_private_ca_is_trusted() {
        let roots = Roots::from_bundles(
            Some(Ok(CA.to_vec())),
            Some(("/etc/ssl/cert.pem", CA.to_vec())),
        );
        let RootCerts::Specific(trusted) = roots.root_certs() else {
            panic!("the bundle did not replace the roots: {roots:?}");
        };
        assert_eq!(trusted.len(), 2);
        assert_eq!(
            roots.describe(),
            "SSL_CERT_FILE (1 certificate) plus /etc/ssl/cert.pem"
        );
        assert!(roots.warning().is_none());
    }

    #[test]
    fn an_unusable_bundle_keeps_the_built_in_roots_and_warns() {
        let unreadable = Roots::from_bundles(Some(Err("no such file".to_owned())), None);
        assert!(matches!(unreadable.root_certs(), RootCerts::WebPki));
        assert_eq!(
            unreadable.warning(),
            Some(
                "SSL_CERT_FILE could not be read (no such file); using the built-in certificate roots"
            )
        );

        let empty = Roots::from_bundles(Some(Ok(b"not a certificate".to_vec())), None);
        assert!(matches!(empty.root_certs(), RootCerts::WebPki));
        assert_eq!(
            empty.warning(),
            Some("SSL_CERT_FILE holds no certificates; using the built-in certificate roots")
        );

        let unset = Roots::from_bundles(None, None);
        assert!(matches!(unset.root_certs(), RootCerts::WebPki));
        assert_eq!(unset.describe(), "built-in roots");
    }
}
