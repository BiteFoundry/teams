//! Shared HTTP client setup: the roots a TLS connection trusts, and the hint shown when a
//! server certificate is not trusted (typically a TLS-inspecting corporate proxy).
//!
//! Every client trusts the bundled Mozilla roots and the operating system's store (reqwest's
//! `rustls-tls-native-roots`). On top of that, PEM files named by these variables are added:
//!
//! - `TEAMS_EXTRA_CA_PEM`: ours. A path that cannot be read or holds no certificate is an error.
//! - `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`: set for other tools; a bad value is skipped with
//!   one warning so it cannot break `teams`. Note that when `SSL_CERT_FILE` (or `SSL_CERT_DIR`)
//!   is set, the OS store is read from there instead of the platform store.
//!
//! Certificate verification itself cannot be turned off.

use std::{
    collections::HashSet,
    ffi::OsString,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use anyhow::{bail, Result};

pub(crate) const EXTRA_CA_ENV: &str = "TEAMS_EXTRA_CA_PEM";
const CA_ENV_VARS: [&str; 3] = [EXTRA_CA_ENV, "SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS"];
pub(crate) const USER_AGENT: &str = concat!("teams-cli/", env!("CARGO_PKG_VERSION"));

/// Start of the recovery hint, also used to give the error the `tls_untrusted_certificate` code.
pub(crate) const TLS_UNTRUSTED_MARKER: &str = "TLS certificate was not trusted";

/// One configured CA file and what loading it gave.
#[derive(Debug)]
pub(crate) struct CaSource {
    pub var: &'static str,
    pub path: PathBuf,
    pub outcome: std::result::Result<Vec<reqwest::Certificate>, String>,
}

/// A client builder with the shared user agent and the extra roots from the environment.
pub(crate) fn client_builder() -> Result<reqwest::ClientBuilder> {
    client_builder_with(&|name| std::env::var_os(name))
}

/// The configured CA files, in precedence order, with their load result (for `teams doctor`).
pub(crate) fn ca_sources() -> Vec<CaSource> {
    ca_sources_with(&|name| std::env::var_os(name))
}

fn client_builder_with(lookup: &dyn Fn(&str) -> Option<OsString>) -> Result<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder().user_agent(USER_AGENT);
    for source in ca_sources_with(lookup) {
        match source.outcome {
            Ok(certs) => {
                for cert in certs {
                    builder = builder.add_root_certificate(cert);
                }
            }
            Err(err) if source.var == EXTRA_CA_ENV => bail!(
                "{EXTRA_CA_ENV}={}: cannot load CA certificates: {err}",
                source.path.display()
            ),
            Err(err) => warn_once(format!(
                "warning: ignoring {}={}: {err}",
                source.var,
                source.path.display()
            )),
        }
    }
    Ok(builder)
}

fn ca_sources_with(lookup: &dyn Fn(&str) -> Option<OsString>) -> Vec<CaSource> {
    CA_ENV_VARS
        .iter()
        .filter_map(|&var| {
            let raw = lookup(var)?;
            let trimmed = raw.to_string_lossy().trim().to_string();
            if trimmed.is_empty() {
                return None;
            }
            let path = PathBuf::from(trimmed);
            let outcome = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|pem| reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| e.to_string()))
                .and_then(|certs| {
                    if certs.is_empty() {
                        Err("no PEM certificate found".to_string())
                    } else {
                        Ok(certs)
                    }
                });
            Some(CaSource { var, path, outcome })
        })
        .collect()
}

/// Prints each warning once per process: several clients are built per command.
fn warn_once(message: String) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    if seen.lock().map(|mut s| s.insert(message.clone())).unwrap_or(false) {
        eprintln!("{message}");
    }
}

/// The recovery hint when `err` failed because the server certificate's issuer is unknown.
pub(crate) fn tls_trust_hint(err: &(dyn std::error::Error + 'static)) -> Option<String> {
    is_unknown_issuer(err).then(|| {
        format!(
            "{TLS_UNTRUSTED_MARKER} (unknown issuer). If your network uses a TLS-inspecting proxy, \
             export the corporate root CA as PEM and set {EXTRA_CA_ENV}=<path> (SSL_CERT_FILE and \
             NODE_EXTRA_CA_CERTS are also honoured). See docs/ENTERPRISE.md."
        )
    })
}

/// `context` for a failed request, followed by the trust hint when it applies.
pub(crate) fn request_context(context: String, err: &reqwest::Error) -> String {
    match tls_trust_hint(err) {
        Some(hint) => format!("{context}. {hint}"),
        None => context,
    }
}

fn is_unknown_issuer(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(err);
    while let Some(e) = current {
        if let Some(tls) = e.downcast_ref::<rustls::Error>() {
            if matches!(
                tls,
                rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)
            ) {
                return true;
            }
        }
        // io::Error::source() skips the error it wraps, so look inside it explicitly.
        if let Some(inner) = e.downcast_ref::<std::io::Error>().and_then(|io| io.get_ref()) {
            if is_unknown_issuer(inner) {
                return true;
            }
        }
        let text = e.to_string().to_ascii_lowercase();
        if text.contains("unknownissuer") || text.contains("unknown issuer") {
            return true;
        }
        current = e.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls/test-ca-bundle.pem")
    }

    fn lookup(vars: &[(&str, PathBuf)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone().into_os_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn extra_ca_bundle_is_loaded_and_builds() {
        let get = lookup(&[(EXTRA_CA_ENV, fixture())]);
        let sources = ca_sources_with(&get);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].outcome.as_ref().map(Vec::len).unwrap(), 2);
        client_builder_with(&get).unwrap().build().unwrap();
    }

    #[test]
    fn bad_teams_extra_ca_is_an_error_naming_the_variable() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = dir.path().join("garbage.pem");
        std::fs::write(&garbage, "not a certificate").unwrap();
        for path in [garbage, dir.path().join("missing.pem")] {
            let err = client_builder_with(&lookup(&[(EXTRA_CA_ENV, path.clone())])).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains(EXTRA_CA_ENV) && msg.contains(&path.display().to_string()), "{msg}");
        }
    }

    #[test]
    fn bad_borrowed_ca_variables_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = dir.path().join("garbage.pem");
        std::fs::write(&garbage, "not a certificate").unwrap();
        let get = lookup(&[
            ("NODE_EXTRA_CA_CERTS", garbage),
            ("SSL_CERT_FILE", dir.path().join("missing.pem")),
        ]);
        client_builder_with(&get).unwrap().build().unwrap();
        assert!(ca_sources_with(&get).iter().all(|s| s.outcome.is_err()));

        // All set variables are additive, in precedence order.
        let get = lookup(&[("NODE_EXTRA_CA_CERTS", fixture()), (EXTRA_CA_ENV, fixture())]);
        let vars: Vec<&str> = ca_sources_with(&get).iter().map(|s| s.var).collect();
        assert_eq!(vars, [EXTRA_CA_ENV, "NODE_EXTRA_CA_CERTS"]);
        assert!(ca_sources_with(&lookup(&[])).is_empty());
    }

    #[test]
    fn trust_hint_only_for_unknown_issuer() {
        #[derive(Debug)]
        struct Wrap(String, Option<Box<dyn std::error::Error + Send + Sync>>);
        impl std::fmt::Display for Wrap {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::error::Error for Wrap {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1.as_deref().map(|e| e as &(dyn std::error::Error + 'static))
            }
        }

        // reqwest -> hyper -> io::Error(rustls::Error), as a real handshake failure nests it.
        let tls = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        let io = std::io::Error::new(std::io::ErrorKind::InvalidData, tls);
        let chain = Wrap("error sending request".into(), Some(Box::new(Wrap("client error (Connect)".into(), Some(Box::new(io))))));
        let hint = tls_trust_hint(&chain).expect("unknown issuer gets the hint");
        assert!(hint.starts_with(TLS_UNTRUSTED_MARKER) && hint.contains(EXTRA_CA_ENV), "{hint}");

        // Other certificate problems and transport errors do not.
        for other in [
            rustls::Error::InvalidCertificate(rustls::CertificateError::Expired),
            rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName),
        ] {
            let io = std::io::Error::new(std::io::ErrorKind::InvalidData, other);
            assert!(tls_trust_hint(&Wrap("error sending request".into(), Some(Box::new(io)))).is_none());
        }
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(tls_trust_hint(&Wrap("error sending request".into(), Some(Box::new(refused)))).is_none());
        let timeout = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert!(tls_trust_hint(&timeout).is_none());
    }
}
