//! Wire-format conformance suite for the Webcash flavor.
//!
//! Three layers gate merges that touch protocol-relevant code:
//! 1. **Static fixtures** — request/response captures from production
//!    `https://webcash.org`, byte-for-byte exact. M1 adds an HTTP-level
//!    comparator that boots `server-webcash` and replays each fixture.
//! 2. **Property tests** (proptest) — wire-format round-trips, amount
//!    overflow, hash determinism. Stubs in `prop`; populated in M1.
//! 3. **Live smoke harness** (`live-webcash-org` feature) — boots
//!    `server-webcash` against an empty Redis, mines testnet webcash, and
//!    cross-checks against production webcash.org. Stub in `live_smoke`;
//!    populated in M1.

pub mod fixtures {
    //! Loader for the captured production fixtures in `fixtures/webcash_org_production/`.

    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    /// One captured request/response pair. Mirrors the JSON layout used in
    /// `fixtures/webcash_org_production/*.json`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Fixture {
        /// Wall-clock when this request/response was captured.
        pub captured_at: String,
        /// Captured HTTP request that was sent to webcash.org.
        pub request: Request,
        /// Captured HTTP response.
        pub response: Response,
        /// Free-form notes about quirks worth preserving.
        #[serde(default)]
        pub notes: Vec<String>,
    }

    /// Captured HTTP request.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Request {
        /// HTTP method (`GET`, `POST`, ...).
        pub method: String,
        /// Full request URL.
        pub url: String,
        /// Request headers as captured.
        #[serde(default)]
        pub headers: BTreeMap<String, String>,
        /// Parsed JSON body (when applicable).
        #[serde(default)]
        pub body: Option<serde_json::Value>,
        /// Raw body bytes (when JSON parse wasn't applicable / wanted).
        #[serde(default)]
        pub body_raw: Option<String>,
    }

    /// Captured HTTP response.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Response {
        /// HTTP status code.
        pub status: u16,
        /// Response headers as captured (Content-Type drives the
        /// Tornado-quirk invariants).
        #[serde(default)]
        pub headers: BTreeMap<String, String>,
        /// Raw response body bytes.
        #[serde(default)]
        pub body_raw: Option<String>,
        /// Parsed JSON body (when applicable).
        #[serde(default)]
        pub body_parsed: Option<serde_json::Value>,
        /// Only present for fixtures whose body is stored in a sibling file
        /// (e.g., the multi-KB Terms of Service text).
        #[serde(default)]
        pub body_file: Option<String>,
        /// SHA256 of the response body, hex-encoded.
        #[serde(default)]
        pub body_sha256: Option<String>,
        /// Response body length in bytes.
        #[serde(default)]
        pub body_length: Option<usize>,
    }

    /// Default location of the production fixtures dir, relative to the
    /// `webycash-conformance` crate root.
    pub fn production_dir() -> PathBuf {
        // CARGO_MANIFEST_DIR is set at compile time for this crate.
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join("webcash_org_production")
    }

    /// Load a single fixture by stem (e.g., `"get_target"`).
    pub fn load(stem: &str) -> Result<Fixture, FixtureError> {
        let path = production_dir().join(format!("{stem}.json"));
        let bytes = std::fs::read(&path).map_err(|e| FixtureError::Io(path.clone(), e))?;
        let fx: Fixture = serde_json::from_slice(&bytes)
            .map_err(|e| FixtureError::Parse(path, e))?;
        Ok(fx)
    }

    /// Load every fixture in the production dir. Skips non-`.json` files.
    pub fn load_all() -> Result<Vec<(String, Fixture)>, FixtureError> {
        let dir = production_dir();
        let mut out = Vec::new();
        let entries = std::fs::read_dir(&dir).map_err(|e| FixtureError::Io(dir.clone(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| FixtureError::Io(dir.clone(), e))?;
            let path = entry.path();
            let Some(ext) = path.extension() else { continue };
            if ext != "json" {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .ok_or_else(|| FixtureError::BadName(path.clone()))?;
            let fx = load(&stem)?;
            out.push((stem, fx));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Failure modes when loading a fixture from disk.
    #[derive(Debug, thiserror::Error)]
    pub enum FixtureError {
        /// `std::fs::read` failed for the given path.
        #[error("io error reading {0}: {1}")]
        Io(PathBuf, std::io::Error),
        /// `serde_json::from_slice` failed for the given path.
        #[error("parse error in {0}: {1}")]
        Parse(PathBuf, serde_json::Error),
        /// Path didn't have a parseable file stem.
        #[error("invalid fixture filename: {0}")]
        BadName(PathBuf),
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn fixtures_dir_exists_and_loads() {
            let all = load_all().expect("fixtures must load");
            assert!(
                all.len() >= 6,
                "expected at least 6 captured fixtures, got {}",
                all.len()
            );
            for (stem, fx) in &all {
                assert!(!fx.captured_at.is_empty(), "{stem}: missing captured_at");
                assert!(!fx.request.method.is_empty(), "{stem}: missing method");
                assert!(!fx.request.url.is_empty(), "{stem}: missing url");
                assert!(fx.response.status >= 100, "{stem}: bogus status");
            }
        }

        #[test]
        fn target_fixture_has_expected_shape() {
            let fx = load("get_target").expect("get_target.json must exist");
            assert_eq!(fx.request.method, "GET");
            assert_eq!(fx.response.status, 200);
            // Production quirk we must preserve in M1.
            assert_eq!(
                fx.response.headers.get("Content-Type").map(String::as_str),
                Some("text/html; charset=UTF-8"),
                "production webcash.org returns text/html for JSON bodies"
            );
            let parsed = fx
                .response
                .body_parsed
                .as_ref()
                .expect("get_target body must parse");
            assert!(parsed.get("difficulty_target_bits").is_some());
            assert!(parsed.get("ratio").is_some());
        }
    }
}

pub mod prop {
    //! Property-test generators and shared assertions.
    //!
    //! Land in M1 once `webycash-proto::parse_secret` and friends exist.
    //! Until then this module is a placeholder so the crate's module tree
    //! is stable.
}

#[cfg(feature = "live-webcash-org")]
pub mod live_smoke {
    //! Live cross-check against `https://webcash.org`. Disabled by default.
    //!
    //! Lands in M1.
}
