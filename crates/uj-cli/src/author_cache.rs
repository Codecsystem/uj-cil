//! Small, content-addressed caches used by the authoring pipeline (Round 14).
//!
//! Everything in here is a *cache*: a missing, corrupted or stale entry must only ever cause
//! extra work (re-hashing, re-validating, re-running the main solution), never a wrong result.
//!
//! * [`FileStamp`] – cheap metadata fingerprint (`size` + `mtime`, plus `ino`/`ctime` on Unix)
//!   with a "racy timestamp" guard: a stamp is only trusted when the file's mtime has
//!   sub-second precision or is at least [`RACY_WINDOW_NS`] older than the moment the stamp was
//!   verified (protects coarse-granularity filesystems such as FAT/SMB).
//! * [`binary_hash`] – process-wide memo of blake3 hashes of compiled binaries.
//! * [`InputHashCache`] – persisted `{hash, stamp}` table for test inputs
//!   (`.uj/cache/input_hashes.json`), so unchanged inputs are not re-hashed on every re-run.
//! * Answer markers – `.uj/cache/ans/<main_hash>/<in_hash>.mark` holding `"<ans_blake3> <len>"`
//!   instead of a second full copy of every `.ans` file.
//! * Validator markers – `.uj/cache/val/<val_hash>/<in_hash>.<group>.ok` (successes only).
//! * Answer state – `.uj/cache/ans_state.json` (`test name -> input hash` the `.ans` was generated
//!   from), consumed by `uj doctor` to detect stale answers.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Stamps whose mtime is closer than this to the verification time are not trusted unless the
/// mtime carries sub-second precision.
pub const RACY_WINDOW_NS: u64 = 2_000_000_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn system_time_ns(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ----------------------------------------------------------------------------
// File stamps
// ----------------------------------------------------------------------------

/// Cheap metadata fingerprint of a regular file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub size: u64,
    pub mtime_ns: u64,
    /// Unix: inode number (0 elsewhere).
    #[serde(default)]
    pub ino: u64,
    /// Unix: status-change time in ns (cannot be forged by `touch -r`; 0 elsewhere).
    #[serde(default)]
    pub ctime_ns: u64,
}

impl FileStamp {
    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        let mtime_ns = meta.modified().map(system_time_ns).unwrap_or(0);
        #[cfg(unix)]
        let (ino, ctime_ns) = {
            use std::os::unix::fs::MetadataExt;
            (
                meta.ino(),
                (meta.ctime().max(0) as u64)
                    .saturating_mul(1_000_000_000)
                    .saturating_add(meta.ctime_nsec().max(0) as u64),
            )
        };
        #[cfg(not(unix))]
        let (ino, ctime_ns) = (0u64, 0u64);
        Self {
            size: meta.len(),
            mtime_ns,
            ino,
            ctime_ns,
        }
    }

    pub fn of(path: &Path) -> io::Result<Self> {
        Ok(Self::from_metadata(&fs::metadata(path)?))
    }

    /// Whether a stamp recorded at `verified_ns` may be trusted for change detection.
    pub fn is_trustworthy(&self, verified_ns: u64) -> bool {
        if self.mtime_ns == 0 {
            return false;
        }
        let has_subsec = !self.mtime_ns.is_multiple_of(1_000_000_000);
        has_subsec || verified_ns.saturating_sub(self.mtime_ns) >= RACY_WINDOW_NS
    }
}

// ----------------------------------------------------------------------------
// Streaming hash helper
// ----------------------------------------------------------------------------

/// blake3 of a file, streamed in 64 KiB chunks (hex encoded).
pub fn hash_file_streaming(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

// ----------------------------------------------------------------------------
// Binary hash memo (process-wide)
// ----------------------------------------------------------------------------

#[derive(Clone)]
struct MemoEntry {
    stamp: FileStamp,
    verified_ns: u64,
    hash: String,
}

static BINARY_HASHES: LazyLock<Mutex<HashMap<PathBuf, MemoEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// blake3 of a compiled binary, memoized per `(path, size, mtime)` for the lifetime of the
/// process (one-shot CLI runs hash each binary once; the MCP server reuses hashes across calls).
pub fn binary_hash(path: &Path) -> io::Result<String> {
    let stamp = FileStamp::of(path)?;
    if let Some(e) = lock(&BINARY_HASHES).get(path).cloned()
        && e.stamp == stamp
        && stamp.is_trustworthy(e.verified_ns)
    {
        return Ok(e.hash);
    }
    let verified_ns = now_ns();
    let hash = hash_file_streaming(path)?;
    // Re-stat: if the binary changed while we were hashing, do not memoize.
    if FileStamp::of(path).ok() == Some(stamp) {
        lock(&BINARY_HASHES).insert(
            path.to_path_buf(),
            MemoEntry {
                stamp,
                verified_ns,
                hash: hash.clone(),
            },
        );
    }
    Ok(hash)
}

// ----------------------------------------------------------------------------
// Persisted input hash cache
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InputEntry {
    hash: String,
    stamp: FileStamp,
    verified_ns: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct InputCacheFile {
    version: u32,
    entries: BTreeMap<String, InputEntry>,
}

const INPUT_CACHE_VERSION: u32 = 1;

/// Persisted `path -> (hash, stamp)` table for test inputs.
///
/// Thread-safe: workers call [`InputHashCache::hash`] / [`InputHashCache::record`] concurrently;
/// the owner calls [`InputHashCache::save`] once at the end of the run.
pub struct InputHashCache {
    problem_dir: PathBuf,
    file: PathBuf,
    entries: Mutex<BTreeMap<String, InputEntry>>,
    dirty: AtomicBool,
    /// When false, stored hashes are never reused (`--no-cache`), but fresh hashes are still
    /// recorded so later runs benefit.
    reuse: bool,
}

impl InputHashCache {
    pub fn cache_path(problem_dir: &Path) -> PathBuf {
        problem_dir
            .join(".uj")
            .join("cache")
            .join("input_hashes.json")
    }

    pub fn load(problem_dir: &Path, reuse: bool) -> Self {
        let file = Self::cache_path(problem_dir);
        let entries = fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<InputCacheFile>(&b).ok())
            .filter(|f| f.version == INPUT_CACHE_VERSION)
            .map(|f| f.entries)
            .unwrap_or_default();
        Self {
            problem_dir: problem_dir.to_path_buf(),
            file,
            entries: Mutex::new(entries),
            dirty: AtomicBool::new(false),
            reuse,
        }
    }

    fn key(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.problem_dir).unwrap_or(path);
        rel.to_string_lossy().replace('\\', "/")
    }

    /// Content hash of `path`, reusing the stored hash when the file stamp is unchanged and
    /// trustworthy; otherwise the file is hashed (and the entry refreshed).
    pub fn hash(&self, path: &Path) -> io::Result<String> {
        let key = self.key(path);
        let stamp = FileStamp::of(path)?;
        if self.reuse
            && let Some(e) = lock(&self.entries).get(&key)
            && e.stamp == stamp
            && stamp.is_trustworthy(e.verified_ns)
        {
            return Ok(e.hash.clone());
        }
        let verified_ns = now_ns();
        let hash = hash_file_streaming(path)?;
        if FileStamp::of(path).ok() == Some(stamp) {
            lock(&self.entries).insert(
                key,
                InputEntry {
                    hash: hash.clone(),
                    stamp,
                    verified_ns,
                },
            );
            self.dirty.store(true, Ordering::Relaxed);
        }
        Ok(hash)
    }

    /// Records a hash that is already known (e.g. computed from the bytes a generator just wrote).
    pub fn record(&self, path: &Path, hash: &str) {
        let verified_ns = now_ns();
        if let Ok(stamp) = FileStamp::of(path) {
            lock(&self.entries).insert(
                self.key(path),
                InputEntry {
                    hash: hash.to_string(),
                    stamp,
                    verified_ns,
                },
            );
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Persists the table (atomic write) if anything changed. Entries for files that no longer
    /// exist are pruned.
    pub fn save(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let mut entries = lock(&self.entries).clone();
        entries.retain(|k, _| self.problem_dir.join(k).is_file() || Path::new(k).is_file());
        let data = InputCacheFile {
            version: INPUT_CACHE_VERSION,
            entries,
        };
        if let Ok(bytes) = serde_json::to_vec(&data) {
            let _ = crate::error::atomic_write_bytes(&self.file, &bytes);
        }
    }
}

// ----------------------------------------------------------------------------
// Answer markers
// ----------------------------------------------------------------------------

fn ans_cache_dir(problem_dir: &Path, main_hash: &str) -> PathBuf {
    problem_dir
        .join(".uj")
        .join("cache")
        .join("ans")
        .join(main_hash)
}

pub fn ans_marker_path(problem_dir: &Path, main_hash: &str, in_hash: &str) -> PathBuf {
    ans_cache_dir(problem_dir, main_hash).join(format!("{in_hash}.mark"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AnsMarker {
    ans_hash: String,
    len: u64,
    ans_mtime_ns: u64,
}

fn parse_marker(s: &str) -> Option<AnsMarker> {
    let mut it = s.split_ascii_whitespace();
    let ans_hash = it.next()?.to_string();
    let len = it.next()?.parse().ok()?;
    let ans_mtime_ns = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    if ans_hash.len() != 64 || !ans_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(AnsMarker {
        ans_hash,
        len,
        ans_mtime_ns,
    })
}

/// Returns `true` when `ans_path` is known to hold the main solution's output for the input with
/// hash `in_hash` (cache hit). A size mismatch is always a miss; when the `.ans` file was touched
/// since the marker was written, its content hash is verified against the marker.
pub fn ans_marker_hit(problem_dir: &Path, main_hash: &str, in_hash: &str, ans_path: &Path) -> bool {
    let marker_path = ans_marker_path(problem_dir, main_hash, in_hash);
    let Some(marker) = fs::read_to_string(&marker_path)
        .ok()
        .and_then(|s| parse_marker(&s))
    else {
        return false;
    };
    let Ok(stamp) = FileStamp::of(ans_path) else {
        return false;
    };
    if stamp.size != marker.len {
        return false;
    }
    if marker.ans_mtime_ns != 0 && stamp.mtime_ns == marker.ans_mtime_ns {
        return true;
    }
    // The .ans file was touched (or the marker predates mtime tracking): verify content.
    match hash_file_streaming(ans_path) {
        Ok(h) if h == marker.ans_hash => {
            let _ = write_ans_marker_raw(&marker_path, &marker.ans_hash, stamp.size, ans_path);
            true
        }
        _ => false,
    }
}

fn write_ans_marker_raw(
    marker_path: &Path,
    ans_hash: &str,
    len: u64,
    ans_path: &Path,
) -> io::Result<()> {
    let mtime_ns = FileStamp::of(ans_path).map(|s| s.mtime_ns).unwrap_or(0);
    crate::error::atomic_write_bytes(
        marker_path,
        format!("{ans_hash} {len} {mtime_ns}\n").as_bytes(),
    )
}

/// Writes the tiny marker for a freshly generated answer (`ans_bytes` were just written to
/// `ans_path`). Also removes a legacy full-copy cache file (`<in_hash>.ans`) if present.
pub fn write_ans_marker(
    problem_dir: &Path,
    main_hash: &str,
    in_hash: &str,
    ans_path: &Path,
    ans_bytes: &[u8],
) {
    let marker_path = ans_marker_path(problem_dir, main_hash, in_hash);
    let ans_hash = blake3::hash(ans_bytes).to_hex().to_string();
    let _ = write_ans_marker_raw(&marker_path, &ans_hash, ans_bytes.len() as u64, ans_path);
    let legacy = ans_cache_dir(problem_dir, main_hash).join(format!("{in_hash}.ans"));
    if legacy.exists() {
        let _ = fs::remove_file(legacy);
    }
}

// ----------------------------------------------------------------------------
// Validator markers
// ----------------------------------------------------------------------------

pub fn validator_marker_path(
    problem_dir: &Path,
    val_hash: &str,
    in_hash: &str,
    group: Option<u32>,
) -> PathBuf {
    let group_s = group.map_or_else(|| "all".to_string(), |g| format!("g{g}"));
    problem_dir
        .join(".uj")
        .join("cache")
        .join("val")
        .join(val_hash)
        .join(format!("{in_hash}.{group_s}.ok"))
}

pub fn validator_cached_ok(
    problem_dir: &Path,
    val_hash: &str,
    in_hash: &str,
    group: Option<u32>,
) -> bool {
    validator_marker_path(problem_dir, val_hash, in_hash, group).is_file()
}

/// Records a successful validation (failures are never cached).
pub fn record_validator_ok(problem_dir: &Path, val_hash: &str, in_hash: &str, group: Option<u32>) {
    let p = validator_marker_path(problem_dir, val_hash, in_hash, group);
    let _ = crate::error::atomic_write_bytes(&p, b"ok\n");
}

// ----------------------------------------------------------------------------
// Answer state (consumed by `uj doctor`)
// ----------------------------------------------------------------------------

pub fn ans_state_path(problem_dir: &Path) -> PathBuf {
    problem_dir
        .join(".uj")
        .join("cache")
        .join("ans_state.json")
}

pub fn load_ans_state(problem_dir: &Path) -> Option<BTreeMap<String, String>> {
    fs::read(ans_state_path(problem_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

/// Merges `updates` (`test name -> input hash`) into `.uj/cache/ans_state.json`.
pub fn update_ans_state(problem_dir: &Path, updates: &BTreeMap<String, String>) {
    if updates.is_empty() {
        return;
    }
    let mut state = load_ans_state(problem_dir).unwrap_or_default();
    let mut changed = false;
    for (k, v) in updates {
        if state.get(k) != Some(v) {
            state.insert(k.clone(), v.clone());
            changed = true;
        }
    }
    if changed && let Ok(bytes) = serde_json::to_vec(&state) {
        let _ = crate::error::atomic_write_bytes(&ans_state_path(problem_dir), &bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn stamp_trust_rules() {
        let subsec = FileStamp {
            size: 1,
            mtime_ns: 1_000_000_123,
            ino: 0,
            ctime_ns: 0,
        };
        assert!(subsec.is_trustworthy(1_000_000_124));
        let coarse = FileStamp {
            mtime_ns: 5_000_000_000,
            ..subsec
        };
        assert!(!coarse.is_trustworthy(5_500_000_000));
        assert!(coarse.is_trustworthy(7_000_000_000));
    }

    #[test]
    fn input_cache_reuses_and_invalidates_on_change() {
        let d = tmp();
        let p = d.path().join("tests").join("1.in");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"1 2\n").unwrap();

        let c = InputHashCache::load(d.path(), true);
        let h1 = c.hash(&p).unwrap();
        assert_eq!(h1, blake3::hash(b"1 2\n").to_hex().to_string());
        c.save();
        assert!(InputHashCache::cache_path(d.path()).is_file());

        // Poison the stored hash: if the cache is reused the poisoned value comes back, which
        // proves no re-hash happened (only possible when the stamp is trustworthy).
        let mut raw: serde_json::Value =
            serde_json::from_slice(&fs::read(InputHashCache::cache_path(d.path())).unwrap())
                .unwrap();
        let entry = &mut raw["entries"]["tests/1.in"];
        entry["hash"] = serde_json::Value::String("poisoned".into());
        let stamp = FileStamp::of(&p).unwrap();
        entry["verified_ns"] = serde_json::json!(stamp.mtime_ns + RACY_WINDOW_NS + 1);
        fs::write(
            InputHashCache::cache_path(d.path()),
            serde_json::to_vec(&raw).unwrap(),
        )
        .unwrap();
        let c2 = InputHashCache::load(d.path(), true);
        assert_eq!(c2.hash(&p).unwrap(), "poisoned");

        // --no-cache: stored hash never reused.
        let c3 = InputHashCache::load(d.path(), false);
        assert_eq!(c3.hash(&p).unwrap(), h1);

        // Same-size rewrite with a different mtime invalidates the entry.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&p, b"3 4\n").unwrap();
        let f = fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(3))
            .unwrap();
        drop(f);
        let c4 = InputHashCache::load(d.path(), true);
        assert_eq!(
            c4.hash(&p).unwrap(),
            blake3::hash(b"3 4\n").to_hex().to_string()
        );
    }

    #[test]
    fn ans_marker_roundtrip_and_size_check() {
        let d = tmp();
        let ans = d.path().join("1.ans");
        fs::write(&ans, b"42\n").unwrap();
        assert!(!ans_marker_hit(d.path(), "m", "i", &ans));
        write_ans_marker(d.path(), "m", "i", &ans, b"42\n");
        let marker = ans_marker_path(d.path(), "m", "i");
        assert!(fs::metadata(&marker).unwrap().len() < 128);
        assert!(ans_marker_hit(d.path(), "m", "i", &ans));

        // Different size -> miss.
        fs::write(&ans, b"420\n").unwrap();
        assert!(!ans_marker_hit(d.path(), "m", "i", &ans));
        // Same size, different content (mtime changed) -> content verified -> miss.
        fs::write(&ans, b"43\n").unwrap();
        assert!(!ans_marker_hit(d.path(), "m", "i", &ans));
        // Restored content (mtime changed) -> content verified -> hit.
        fs::write(&ans, b"42\n").unwrap();
        assert!(ans_marker_hit(d.path(), "m", "i", &ans));
        // Missing ans file -> miss.
        fs::remove_file(&ans).unwrap();
        assert!(!ans_marker_hit(d.path(), "m", "i", &ans));
    }

    #[test]
    fn validator_markers_are_keyed_by_group() {
        let d = tmp();
        assert!(!validator_cached_ok(d.path(), "v", "i", None));
        record_validator_ok(d.path(), "v", "i", Some(2));
        assert!(validator_cached_ok(d.path(), "v", "i", Some(2)));
        assert!(!validator_cached_ok(d.path(), "v", "i", Some(1)));
        assert!(!validator_cached_ok(d.path(), "v", "i", None));
        assert!(!validator_cached_ok(d.path(), "v2", "i", Some(2)));
    }

    #[test]
    fn ans_state_merges() {
        let d = tmp();
        let mut u = BTreeMap::new();
        u.insert("1".to_string(), "aa".to_string());
        update_ans_state(d.path(), &u);
        let mut u2 = BTreeMap::new();
        u2.insert("2".to_string(), "bb".to_string());
        update_ans_state(d.path(), &u2);
        let s = load_ans_state(d.path()).unwrap();
        assert_eq!(s.get("1").map(String::as_str), Some("aa"));
        assert_eq!(s.get("2").map(String::as_str), Some("bb"));
    }

    #[test]
    fn binary_hash_memo_matches_content() {
        let d = tmp();
        let p = d.path().join("a.bin");
        fs::write(&p, b"abc").unwrap();
        let h = binary_hash(&p).unwrap();
        assert_eq!(h, blake3::hash(b"abc").to_hex().to_string());
        assert_eq!(binary_hash(&p).unwrap(), h);
        fs::write(&p, b"abcd").unwrap();
        assert_eq!(
            binary_hash(&p).unwrap(),
            blake3::hash(b"abcd").to_hex().to_string()
        );
    }
}
