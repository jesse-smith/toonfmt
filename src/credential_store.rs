//! Persistent OAuth credential storage for the explicit `login` flow.
//!
//! `login` and `serve` are separate processes, so the token must outlive a single
//! run — rmcp's default `InMemoryCredentialStore` can't bridge them. This is a file
//! store implementing rmcp's [`CredentialStore`] trait, backed by one JSON file per
//! upstream URL under a base directory (production: `~/.toonfmt-auth/`).
//!
//! **Per-(profile, URL) isolation:** the filename is a SHA-256 hex stem (see
//! [`key_hash`]) — `sha256(url)` with no profile (byte-identical to the original
//! per-URL scheme), or `sha256(profile ⊕ "\0" ⊕ url)` with one. Two different
//! upstreams — or two `--profile` identities for the same URL — never share a token
//! file, and neither the URL nor the profile appears on the filesystem (no
//! accidental secret-adjacent path leak; a path-valued profile stays a flat hash).
//!
//! **Permissions:** the token file is `0600` and the base directory `0700` (unix).
//! A bearer/refresh token is a credential; world- or group-readable storage would
//! be a leak. Enforced on every save, not just creation.
//!
//! **Injectable base dir:** [`FileCredentialStore::new`] takes the base directory so
//! tests drive a throwaway path; [`FileCredentialStore::for_url`] resolves the
//! production `~/.toonfmt-auth/` from `$HOME`.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rmcp::transport::{AuthError, CredentialStore, StoredCredentials};
use sha2::{Digest, Sha256};

/// Directory name under `$HOME` for the production store.
const STORE_DIR: &str = ".toonfmt-auth";

/// A file-backed [`CredentialStore`]: one `<sha256(url)>.json` file under `base_dir`.
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    /// Full path to this upstream's credential file.
    path: PathBuf,
    /// The directory holding it (created on save).
    base_dir: PathBuf,
}

impl FileCredentialStore {
    /// Construct a store for `url` (optionally namespaced by `profile`) rooted at
    /// an explicit `base_dir` (injectable for tests). The file is
    /// `<base_dir>/<key_hash(profile, url)>.json` — see [`key_hash`].
    pub fn new(base_dir: impl Into<PathBuf>, url: &str, profile: Option<&str>) -> Self {
        let base_dir = base_dir.into();
        let path = base_dir.join(format!("{}.json", key_hash(profile, url)));
        Self { path, base_dir }
    }

    /// Construct the production store for `url` (optionally namespaced by
    /// `profile`), rooted at `~/.toonfmt-auth/`.
    ///
    /// Errors if `$HOME` is unset (no sensible place to persist).
    pub fn for_url(url: &str, profile: Option<&str>) -> Result<Self> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow!("$HOME is not set; cannot locate the credential store"))?;
        Ok(Self::new(Path::new(&home).join(STORE_DIR), url, profile))
    }

    /// Path to the credential file (exposed for tests / diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// SHA-256 hex digest of the upstream URL — the per-URL filename stem.
fn url_hash(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The credential-file stem for `(profile, url)` — branch on the `Option`, do
/// **not** fold an empty-string default (Gap D).
///
/// - `None` (no `--profile`) → **`url_hash(url)` verbatim**, byte-identical to the
///   pre-profile path. This is load-bearing: any change to this arm silently logs
///   out every existing user. (Pinned by `key_hash_none_matches_frozen_golden`.)
/// - `Some(p)` → `sha256(p ⊕ "\0" ⊕ url)`. The `\0` separator (impossible in a
///   URL or a sane profile name) means `("a", "bc")` can't collide with
///   `("ab", "c")`. Folding into the hash — rather than a `profile/` subdirectory
///   — is what lets a path-valued profile (`/Users/me/project`) work with no
///   sanitization and no nested-dir perms, consistent with why the URL is already
///   hashed (no secret-adjacent path on disk).
fn key_hash(profile: Option<&str>, url: &str) -> String {
    match profile {
        None => url_hash(url),
        Some(p) => {
            let mut hasher = Sha256::new();
            hasher.update(p.as_bytes());
            hasher.update([0u8]);
            hasher.update(url.as_bytes());
            let digest = hasher.finalize();
            let mut hex = String::with_capacity(digest.len() * 2);
            for byte in digest {
                use std::fmt::Write;
                let _ = write!(hex, "{byte:02x}");
            }
            hex
        }
    }
}

/// Map a filesystem/serde failure to rmcp's `AuthError`. The trait constrains the
/// error type, and `InternalError(String)` is the only variant that fits an I/O or
/// (de)serialization fault — it carries the human-readable cause.
fn io_err(context: &str, e: impl std::fmt::Display) -> AuthError {
    AuthError::InternalError(format!("credential store: {context}: {e}"))
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => {
                let creds = serde_json::from_slice::<StoredCredentials>(&bytes)
                    .map_err(|e| io_err("parsing stored credentials", e))?;
                Ok(Some(creds))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err("reading credential file", e)),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        // Base dir must exist and be private before we drop a secret into it.
        tokio::fs::create_dir_all(&self.base_dir)
            .await
            .map_err(|e| io_err("creating store directory", e))?;
        #[cfg(unix)]
        set_mode(&self.base_dir, 0o700)
            .await
            .map_err(|e| io_err("locking down store directory perms", e))?;

        let json = serde_json::to_vec_pretty(&credentials)
            .map_err(|e| io_err("serializing credentials", e))?;
        tokio::fs::write(&self.path, &json)
            .await
            .map_err(|e| io_err("writing credential file", e))?;
        #[cfg(unix)]
        set_mode(&self.path, 0o600)
            .await
            .map_err(|e| io_err("locking down credential file perms", e))?;
        Ok(())
    }

    async fn clear(&self) -> Result<(), AuthError> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err("removing credential file", e)),
        }
    }
}

#[cfg(unix)]
async fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway base dir under the OS temp dir, removed on drop. Avoids a
    /// `tempfile` dependency (which isn't in the offline cargo cache) while keeping
    /// tests hermetic. The unique stem is derived from the test-supplied label, so
    /// concurrent tests don't collide.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(label: &str) -> Self {
            let p = std::env::temp_dir().join(format!("toonfmt-credstore-test-{label}"));
            let _ = std::fs::remove_dir_all(&p);
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn creds(client_id: &str) -> StoredCredentials {
        // token_response: None keeps the fixture independent of oauth2's token
        // types; round-trip fidelity of the envelope (client_id, scopes) is what
        // the store is responsible for.
        StoredCredentials::new(client_id.to_string(), None, vec!["mcp".to_string()], Some(42))
    }

    #[tokio::test]
    async fn missing_file_loads_as_none() {
        let tmp = TempDir::new("missing");
        let store = FileCredentialStore::new(tmp.path(), "https://x.example/mcp", None);
        assert!(store.load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn save_then_load_round_trips() {
        let tmp = TempDir::new("roundtrip");
        let store = FileCredentialStore::new(tmp.path(), "https://x.example/mcp", None);
        store.save(creds("client-abc")).await.unwrap();

        let loaded = store.load().await.unwrap().expect("must be present");
        assert_eq!(loaded.client_id, "client-abc");
        assert_eq!(loaded.granted_scopes, vec!["mcp".to_string()]);
        assert_eq!(loaded.token_received_at, Some(42));
    }

    #[tokio::test]
    async fn clear_removes_then_loads_none() {
        let tmp = TempDir::new("clear");
        let store = FileCredentialStore::new(tmp.path(), "https://x.example/mcp", None);
        store.save(creds("c")).await.unwrap();
        assert!(store.load().await.unwrap().is_some());

        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        // Idempotent: clearing an absent file is Ok.
        store.clear().await.unwrap();
    }

    #[tokio::test]
    async fn distinct_urls_isolate_into_distinct_files() {
        let tmp = TempDir::new("isolation");
        let a = FileCredentialStore::new(tmp.path(), "https://a.example/mcp", None);
        let b = FileCredentialStore::new(tmp.path(), "https://b.example/mcp", None);
        assert_ne!(a.path(), b.path(), "different URLs must map to different files");

        a.save(creds("client-a")).await.unwrap();
        b.save(creds("client-b")).await.unwrap();

        assert_eq!(a.load().await.unwrap().unwrap().client_id, "client-a");
        assert_eq!(b.load().await.unwrap().unwrap().client_id, "client-b");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saved_file_is_0600_and_dir_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("perms");
        let store = FileCredentialStore::new(tmp.path(), "https://x.example/mcp", None);
        store.save(creds("c")).await.unwrap();

        let file_mode = std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "credential file must be private (0600)");

        let dir_mode = std::fs::metadata(tmp.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "store directory must be private (0700)");
    }

    #[test]
    fn url_hash_is_stable_and_hex() {
        let h = url_hash("https://x.example/mcp");
        assert_eq!(h.len(), 64, "sha256 hex is 64 chars");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, url_hash("https://x.example/mcp"), "stable across calls");
        assert_ne!(h, url_hash("https://y.example/mcp"));
    }

    // --- per-profile key derivation (H2, Gap D + Gap F) ---

    /// Gap F (regression-critical): the `None` (no-profile) arm must reproduce
    /// today's `sha256(url)` **byte-for-byte**, anchored to a frozen golden hex —
    /// NOT a self-comparison against `url_hash` (which would pass even if the hash
    /// changed). If this drifts, every existing logged-in user is silently logged
    /// out. Golden: `printf '%s' 'https://x.example/mcp' | shasum -a 256` (no
    /// trailing newline).
    const GOLDEN_NO_PROFILE: &str =
        "7692f47862b33fb9640daca253ada81dc3493105bf505fc78077be6a835b296c";

    #[test]
    fn key_hash_none_matches_frozen_golden() {
        assert_eq!(
            key_hash(None, "https://x.example/mcp"),
            GOLDEN_NO_PROFILE,
            "the no-profile key MUST equal today's sha256(url) — else existing logins break"
        );
    }

    #[test]
    fn key_hash_some_differs_from_golden() {
        // A profile changes the key (so the same URL under a profile is isolated),
        // and it is never accidentally equal to the no-profile key.
        let profiled = key_hash(Some("work"), "https://x.example/mcp");
        assert_ne!(profiled, GOLDEN_NO_PROFILE);
        assert_eq!(profiled.len(), 64);
        assert!(profiled.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn key_hash_distinct_profiles_distinct_keys() {
        let url = "https://x.example/mcp";
        let work = key_hash(Some("work"), url);
        let personal = key_hash(Some("personal"), url);
        assert_ne!(work, personal, "different profiles for one URL must not collide");
    }

    #[test]
    fn key_hash_separator_prevents_concat_collision() {
        // The "\0" separator means profile "a" + url "bc" can't hash-collide with
        // profile "ab" + url "c" (which a bare concatenation would).
        assert_ne!(key_hash(Some("a"), "bc"), key_hash(Some("ab"), "c"));
    }

    #[test]
    fn key_hash_path_valued_profile_is_hashed_flat() {
        // A path-valued profile (--profile ${CLAUDE_PROJECT_DIR}) is hashed like
        // any other string — no slashes reach the filesystem, no nested dirs.
        let h = key_hash(Some("/Users/me/project"), "https://x.example/mcp");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "stem is flat hex, no '/'");
    }

    #[test]
    fn distinct_profiles_isolate_into_distinct_files() {
        let tmp = TempDir::new("profile-isolation");
        let url = "https://x.example/mcp";
        let shared = FileCredentialStore::new(tmp.path(), url, None);
        let work = FileCredentialStore::new(tmp.path(), url, Some("work"));
        let personal = FileCredentialStore::new(tmp.path(), url, Some("personal"));
        assert_ne!(shared.path(), work.path());
        assert_ne!(work.path(), personal.path());
        assert_ne!(shared.path(), personal.path());
    }
}
