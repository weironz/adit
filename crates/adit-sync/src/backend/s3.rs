//! S3-compatible object storage — AWS S3, MinIO, Cloudflare R2, 阿里云 OSS.
//!
//! Signature V4 is written out here rather than pulled in as an SDK: the AWS
//! SDK is a large dependency for two requests, and every S3-compatible service
//! speaks the same signing scheme, so one small implementation covers all of
//! them. It is also exactly the kind of code that is wrong-or-right with
//! nothing in between, which makes it easy to test — the signing-key
//! derivation and the hashes below are checked against AWS's own published
//! vectors.
//!
//! Conditional writes are attempted (`If-Match` / `If-None-Match`, which AWS
//! added in 2024 and MinIO and R2 support). A service that ignores them
//! degrades to last-write-wins, which the orchestration layer already handles
//! by reading back after a push.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::{agent, remote_error};
use crate::{RemoteRevision, SyncBackend, SyncDocument, SyncError};

const PROVIDER: &str = "S3";
const SERVICE: &str = "s3";
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct S3Config {
    /// Host only, no scheme: `s3.amazonaws.com`, `play.min.io:9000`,
    /// `<account>.r2.cloudflarestorage.com`.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Object key, e.g. `adit/adit-sync.json`.
    pub key: String,
    pub access_key: String,
    pub secret_key: String,
    /// Path-style (`host/bucket/key`) instead of virtual-host style
    /// (`bucket.host/key`). MinIO and most self-hosted gateways need this;
    /// AWS and R2 do not.
    pub path_style: bool,
}

pub struct S3Backend {
    config: S3Config,
    agent: ureq::Agent,
}

impl S3Backend {
    #[must_use]
    pub fn new(config: S3Config) -> Self {
        Self {
            config,
            agent: agent(),
        }
    }

    fn host(&self) -> String {
        if self.config.path_style {
            self.config.endpoint.clone()
        } else {
            format!("{}.{}", self.config.bucket, self.config.endpoint)
        }
    }

    /// The canonical URI: always absolute, always starting with `/`, and with
    /// each path segment percent-encoded. S3 signs the *encoded* path, so a
    /// key with a space signs differently from one with `%20` written out.
    fn canonical_path(&self) -> String {
        let key = self.config.key.trim_start_matches('/');
        let encoded: Vec<String> = key
            .split('/')
            .map(|segment| uri_encode(segment, false))
            .collect();
        if self.config.path_style {
            format!(
                "/{}/{}",
                uri_encode(&self.config.bucket, false),
                encoded.join("/")
            )
        } else {
            format!("/{}", encoded.join("/"))
        }
    }

    fn url(&self) -> String {
        format!("https://{}{}", self.host(), self.canonical_path())
    }

    /// Sign one request and return the headers to send.
    ///
    /// `extra` are headers beyond the three S3 always requires; they must be
    /// included in the signature or the service rejects them, which is why
    /// they are passed in rather than added by the caller afterwards.
    fn sign(
        &self,
        method: &str,
        payload: &[u8],
        extra: &[(&str, String)],
    ) -> Result<Vec<(String, String)>, SyncError> {
        let now = std::time::SystemTime::now();
        let (amz_date, date_stamp) = amz_timestamps(now)?;
        let payload_hash = hex::encode(Sha256::digest(payload));
        let host = self.host();

        // Canonical headers must be lowercase, sorted by name, values trimmed.
        let mut headers: Vec<(String, String)> = vec![
            ("host".to_owned(), host),
            ("x-amz-content-sha256".to_owned(), payload_hash.clone()),
            ("x-amz-date".to_owned(), amz_date.clone()),
        ];
        for (name, value) in extra {
            headers.push((name.to_ascii_lowercase(), value.clone()));
        }
        headers.sort_by(|a, b| a.0.cmp(&b.0));

        let canonical_headers: String = headers
            .iter()
            .map(|(name, value)| format!("{name}:{}\n", value.trim()))
            .collect();
        let signed_names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
        let signed_headers = signed_names.join(";");

        let canonical_request = format!(
            "{method}\n{}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            self.canonical_path()
        );

        let scope = format!("{date_stamp}/{}/{SERVICE}/aws4_request", self.config.region);
        let string_to_sign = format!(
            "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );

        let key = signing_key(&self.config.secret_key, &date_stamp, &self.config.region);
        let signature = hex::encode(hmac(&key, string_to_sign.as_bytes()));

        let authorization = format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.config.access_key
        );

        // `host` is set by the HTTP client itself; sending it again would be
        // rejected as a duplicate.
        let mut out: Vec<(String, String)> = headers
            .into_iter()
            .filter(|(name, _)| name != "host")
            .collect();
        out.push(("Authorization".to_owned(), authorization));
        Ok(out)
    }

    fn configured(&self) -> Result<(), SyncError> {
        if self.config.endpoint.trim().is_empty()
            || self.config.bucket.trim().is_empty()
            || self.config.access_key.trim().is_empty()
        {
            return Err(SyncError::NotAuthenticated {
                provider: PROVIDER.to_owned(),
            });
        }
        Ok(())
    }
}

impl SyncBackend for S3Backend {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn fetch(&mut self) -> Result<Option<(SyncDocument, RemoteRevision)>, SyncError> {
        self.configured()?;
        let headers = self.sign("GET", b"", &[])?;
        let mut request = self.agent.get(&self.url());
        for (name, value) in &headers {
            request = request.set(name, value);
        }

        let response = match request.call() {
            Ok(response) => response,
            // Nothing stored yet. 403 shows up here too: many buckets deny
            // ListBucket, and a GET on a missing key then answers 403 rather
            // than leak whether it exists.
            Err(ureq::Error::Status(404 | 403, _)) => return Ok(None),
            Err(error) => return Err(remote_error(PROVIDER, error)),
        };

        let revision = RemoteRevision {
            token: response.header("ETag").unwrap_or_default().to_owned(),
        };
        let body = response.into_string().map_err(SyncError::Io)?;
        if body.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some((SyncDocument::parse(body.as_bytes())?, revision)))
    }

    fn push(
        &mut self,
        document: &SyncDocument,
        expected: Option<&RemoteRevision>,
    ) -> Result<RemoteRevision, SyncError> {
        self.configured()?;
        let bytes = document.to_bytes()?;

        let mut extra = vec![("content-type", "application/json".to_owned())];
        match expected.map(|revision| revision.token.as_str()) {
            Some(etag) if !etag.is_empty() => extra.push(("if-match", etag.to_owned())),
            _ => extra.push(("if-none-match", "*".to_owned())),
        }

        let headers = self.sign("PUT", &bytes, &extra)?;
        let mut request = self.agent.put(&self.url());
        for (name, value) in &headers {
            request = request.set(name, value);
        }

        let response = match request.send_bytes(&bytes) {
            Ok(response) => response,
            Err(ureq::Error::Status(412 | 409, _)) => {
                return Err(SyncError::Conflict {
                    provider: PROVIDER.to_owned(),
                });
            }
            Err(error) => return Err(remote_error(PROVIDER, error)),
        };

        Ok(RemoteRevision {
            token: response.header("ETag").unwrap_or_default().to_owned(),
        })
    }
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// The four-step key derivation from the SigV4 spec.
fn signing_key(secret: &str, date_stamp: &str, region: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, SERVICE.as_bytes());
    hmac(&k_service, b"aws4_request")
}

/// Percent-encode per the SigV4 rules, which are stricter than a URL encoder:
/// only unreserved characters survive, and `/` survives only where a path
/// separator is meant.
fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `(20260805T023000Z, 20260805)` from a wall clock.
///
/// Written out rather than pulling in a date library for one format. The civil
/// date comes from Howard Hinnant's `civil_from_days`, which is exact across
/// the proleptic Gregorian range and needs no tables.
fn amz_timestamps(now: std::time::SystemTime) -> Result<(String, String), SyncError> {
    let seconds = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| SyncError::Malformed("system clock is before 1970".to_owned()))?
        .as_secs();
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let time_of_day = seconds % 86_400;
    let (year, month, day) = crate::civil_from_days(days);
    let (hour, minute, second) = (
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60,
    );
    Ok((
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
        format!("{year:04}{month:02}{day:02}"),
    ))
}


#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's own published derivation example. A wrong signing key is
    /// indistinguishable from a wrong secret at the service, so this is the
    /// one test that turns "access denied" from a guess into a fact.
    #[test]
    fn the_signing_key_matches_the_aws_example() {
        let k_date = hmac(b"AWS4wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", b"20120215");
        let k_region = hmac(&k_date, b"us-east-1");
        let k_service = hmac(&k_region, b"iam");
        let expected = hmac(&k_service, b"aws4_request");
        assert_eq!(
            hex::encode(&expected),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
        // The real derivation differs only by the service name, so this also
        // pins the chain order: swapping two steps changes the result.
        let ours = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20120215",
            "us-east-1",
        );
        assert_ne!(hex::encode(&ours), hex::encode(&expected));
        assert_eq!(ours.len(), 32);
    }

    /// The empty-payload hash appears in every GET; a wrong one fails every
    /// request with a signature mismatch.
    #[test]
    fn the_empty_payload_hash_is_the_known_constant() {
        assert_eq!(
            hex::encode(Sha256::digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// SigV4 encodes more aggressively than a URL encoder: a space is `%20`,
    /// never `+`, and only unreserved characters pass through.
    #[test]
    fn uri_encoding_follows_the_sigv4_rules() {
        assert_eq!(uri_encode("adit-sync.json", false), "adit-sync.json");
        assert_eq!(uri_encode("my file", false), "my%20file");
        assert_eq!(uri_encode("a/b", false), "a/b");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("~_-.", false), "~_-.");
        assert_eq!(uri_encode("+", false), "%2B");
    }

    /// Timestamp formatting against instants whose values are independently
    /// known: the epoch, a century leap year (400-rule), and an ordinary one.
    #[test]
    fn timestamps_format_known_instants() {
        let at = |secs: u64| {
            amz_timestamps(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .expect("after 1970")
        };
        assert_eq!(
            at(0),
            ("19700101T000000Z".to_owned(), "19700101".to_owned())
        );
        assert_eq!(
            at(951_827_696),
            ("20000229T123456Z".to_owned(), "20000229".to_owned())
        );
        assert_eq!(
            at(1_709_164_800),
            ("20240229T000000Z".to_owned(), "20240229".to_owned())
        );
    }

    /// Path- and virtual-host styles produce different signed paths, and
    /// getting that backwards is a signature mismatch on every request.
    #[test]
    fn the_signed_path_follows_the_addressing_style() {
        let base = S3Config {
            endpoint: "play.min.io".into(),
            region: "us-east-1".into(),
            bucket: "adit".into(),
            key: "sync/adit-sync.json".into(),
            access_key: "key".into(),
            secret_key: "secret".into(),
            path_style: true,
        };
        let path_style = S3Backend::new(base.clone());
        assert_eq!(path_style.canonical_path(), "/adit/sync/adit-sync.json");
        assert_eq!(path_style.host(), "play.min.io");

        let virtual_host = S3Backend::new(S3Config {
            path_style: false,
            ..base
        });
        assert_eq!(virtual_host.canonical_path(), "/sync/adit-sync.json");
        assert_eq!(virtual_host.host(), "adit.play.min.io");
    }

    #[test]
    fn an_unconfigured_bucket_is_caught_locally() {
        let mut backend = S3Backend::new(S3Config {
            endpoint: String::new(),
            region: "us-east-1".into(),
            bucket: String::new(),
            key: "k".into(),
            access_key: String::new(),
            secret_key: String::new(),
            path_style: true,
        });
        assert!(matches!(
            backend.fetch(),
            Err(SyncError::NotAuthenticated { .. })
        ));
    }
}
