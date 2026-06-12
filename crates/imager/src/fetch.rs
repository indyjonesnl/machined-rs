//! Checksum-verified, cached downloads. The cache key is the pinned sha256,
//! and a cache hit is re-hashed before being trusted: an entry that no longer
//! matches its pin is discarded and re-downloaded.

use anyhow::Context;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

pub trait Fetch {
    fn get(&self, url: &str) -> anyhow::Result<Vec<u8>>;
}

// wired in Task 10
#[allow(dead_code)]
pub struct HttpFetcher;

impl Fetch for HttpFetcher {
    fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let resp = ureq::get(url).call()?;
        let mut buf = Vec::new();
        resp.into_reader().read_to_end(&mut buf)?;
        Ok(buf)
    }
}

/// Return a path to the verified artifact, downloading only on cache miss.
/// A cache hit is re-hashed before it is trusted; a stale or poisoned entry
/// is silently discarded and re-downloaded.
///
/// # Errors
///
/// Fails if the pin is not 64 lowercase hex chars, if the download fails, if
/// the downloaded bytes do not hash to the pin (a checksum mismatch is a hard
/// error and leaves no cache entry behind), or on cache-dir I/O errors.
// wired in Task 10
#[allow(dead_code)]
pub fn fetch_verified(
    fetcher: &dyn Fetch,
    url: &str,
    sha256: &str,
    cache_dir: &Path,
) -> anyhow::Result<PathBuf> {
    // Guard the pin format before it is used as a path component or compared
    // against lowercase hex digests.
    if sha256.len() != 64
        || !sha256.bytes().all(|b| b.is_ascii_hexdigit())
        || sha256 != sha256.to_lowercase()
    {
        anyhow::bail!("invalid sha256 pin for {url}: must be 64 lowercase hex chars");
    }
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;
    let cached = cache_dir.join(sha256);
    if cached.exists() {
        let existing = std::fs::read(&cached)
            .with_context(|| format!("reading cached artifact {}", cached.display()))?;
        if hex::encode(Sha256::digest(&existing)) == sha256 {
            return Ok(cached);
        }
        // Poisoned or corrupt entry: drop it and fall through to a fresh download.
        std::fs::remove_file(&cached)
            .with_context(|| format!("removing corrupt cache entry {}", cached.display()))?;
    }
    let body = fetcher.get(url)?;
    let actual = hex::encode(Sha256::digest(&body));
    if actual != sha256 {
        anyhow::bail!("sha256 mismatch for {url}: expected {sha256}, got {actual}");
    }
    // Write-then-rename so a crash never leaves a half-written "verified" file.
    let tmp = cache_dir.join(format!(".{sha256}.tmp"));
    std::fs::write(&tmp, &body)
        .with_context(|| format!("writing artifact to {}", tmp.display()))?;
    std::fs::rename(&tmp, &cached)
        .with_context(|| format!("moving artifact into place at {}", cached.display()))?;
    Ok(cached)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    struct StaticFetcher(Vec<u8>);
    impl Fetch for StaticFetcher {
        fn get(&self, _url: &str) -> anyhow::Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn good_checksum_is_cached_and_returned() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"payload".to_vec();
        let sum = hex::encode(Sha256::digest(&body));
        let p =
            fetch_verified(&StaticFetcher(body.clone()), "http://x/a", &sum, dir.path()).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), body);
        // Second call hits cache: the entry is re-hashed locally, and a
        // fetcher that would now fail is never asked.
        struct Bomb;
        impl Fetch for Bomb {
            fn get(&self, _u: &str) -> anyhow::Result<Vec<u8>> {
                panic!("must not re-download")
            }
        }
        let p2 = fetch_verified(&Bomb, "http://x/a", &sum, dir.path()).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn poisoned_cache_entry_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"payload".to_vec();
        let sum = hex::encode(Sha256::digest(&body));
        // Something else planted garbage at the cache path for our pin.
        std::fs::write(dir.path().join(&sum), b"garbage").unwrap();
        let p =
            fetch_verified(&StaticFetcher(body.clone()), "http://x/a", &sum, dir.path()).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), body);
    }

    #[test]
    fn malformed_pins_are_rejected_and_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"payload".to_vec();
        let upper = hex::encode(Sha256::digest(&body)).to_uppercase();
        for pin in [upper.as_str(), "../evil", "aa"] {
            let err = fetch_verified(&StaticFetcher(body.clone()), "http://x/a", pin, dir.path())
                .unwrap_err();
            assert!(err.to_string().contains("invalid sha256 pin"), "{err}");
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn bad_checksum_is_a_hard_error_and_nothing_is_cached() {
        let dir = tempfile::tempdir().unwrap();
        let err = fetch_verified(
            &StaticFetcher(b"evil".to_vec()),
            "http://x/a",
            &"00".repeat(32),
            dir.path(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"), "{err}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
