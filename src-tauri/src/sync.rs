//! Keeps a local copy of the game files in sync with the server.
//!
//! The server publishes `GET /manifest.json`: one entry per file with its size
//! and SHA-256. On every start each local file is compared against that, and
//! only files whose hash differs are downloaded. A file that already matches is
//! left exactly as it is, which is what makes a second start instant.
//!
//! Hashing the whole tree every launch would cost as much as the download it
//! is meant to avoid, so each file's hash is memoized in a state file keyed by
//! (size, mtime). A file the server would also call unchanged is therefore
//! compared with two `stat` fields instead of being re-read.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const STATE_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateEntry {
    size: u64,
    mtime_ms: u64,
    sha256: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    version: u32,
    files: BTreeMap<String, StateEntry>,
}

#[derive(Debug, Default, Clone)]
pub struct SyncResult {
    pub downloaded: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub bytes: u64,
    #[allow(dead_code)]
    pub files: usize,
}

/// Progress callback: (done, total, bytes, bytes_total, current path).
pub type Progress<'a> = &'a mut dyn FnMut(usize, usize, u64, u64, &str);

fn join_url(base: &str, rel: &str) -> String {
    let base = base.trim_end_matches('/');
    let encoded: Vec<String> = rel
        .split('/')
        .map(|seg| {
            // Percent-encode the characters that are illegal in a path
            // segment. Map ids contain spaces; `#` and `?` would truncate.
            let mut out = String::new();
            for b in seg.bytes() {
                match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        out.push(b as char)
                    }
                    _ => out.push_str(&format!("%{:02X}", b)),
                }
            }
            out
        })
        .collect();
    format!("{}/{}", base, encoded.join("/"))
}

/// Reject anything that would land outside the target directory: the manifest
/// comes off the network, so it is not trusted to stay inside the folder.
fn safe_target(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let abs = root.join(rel);
    let abs_norm = normalize(&abs);
    let root_norm = normalize(root);
    if abs_norm != root_norm && !abs_norm.starts_with(&root_norm) {
        return Err(format!("refusing path outside the game folder: {rel}"));
    }
    Ok(abs)
}

/// Lexical normalisation: resolves `.` and `..` without touching the disk.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        use std::path::Component::*;
        match comp {
            CurDir => {}
            ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn mtime_ms(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn load_state(path: &Path) -> BTreeMap<String, StateEntry> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return BTreeMap::new(),
    };
    match serde_json::from_str::<State>(&raw) {
        Ok(state) if state.version == STATE_VERSION => state.files,
        // Missing or from an older layout: start cold, hashes are recomputed.
        _ => BTreeMap::new(),
    }
}

fn save_state(path: &Path, files: BTreeMap<String, StateEntry>) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let state = State {
        version: STATE_VERSION,
        files,
    };
    if let Ok(json) = serde_json::to_string(&state) {
        let _ = fs::write(path, json);
    }
}

fn fetch_manifest(server: &str, agent: &ureq::Agent) -> Result<Vec<ManifestFile>, String> {
    let url = format!("{}/manifest.json", server.trim_end_matches('/'));
    let res = agent.get(&url).call().map_err(|e| format!("manifest: {e}"))?;
    let status = res.status();
    if !(200..300).contains(&status) {
        return Err(format!("manifest: HTTP {status}"));
    }
    let body = res
        .into_string()
        .map_err(|e| format!("manifest: {e}"))?;
    let manifest: Manifest =
        serde_json::from_str(&body).map_err(|e| format!("manifest: {e}"))?;
    Ok(manifest.files)
}

fn download_to(
    agent: &ureq::Agent,
    server: &str,
    rel: &str,
    abs: &Path,
    on_bytes: &mut dyn FnMut(u64),
) -> Result<(), String> {
    let url = join_url(server, rel);
    let res = agent.get(&url).call().map_err(|e| format!("{rel}: {e}"))?;
    let status = res.status();
    if !(200..300).contains(&status) {
        return Err(format!("{rel}: HTTP {status}"));
    }
    let mut body = res.into_reader();

    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{rel}: {e}"))?;
    }
    let tmp = abs.with_extension(format!(
        "{}.part",
        abs.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));

    {
        let mut out = fs::File::create(&tmp).map_err(|e| format!("{rel}: {e}"))?;
        let mut buf = vec![0u8; 128 * 1024];
        loop {
            let n = body.read(&mut buf).map_err(|e| format!("{rel}: {e}"))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(|e| format!("{rel}: {e}"))?;
            on_bytes(n as u64);
        }
        out.flush().map_err(|e| format!("{rel}: {e}"))?;
    }

    fs::rename(&tmp, abs).map_err(|e| format!("{rel}: {e}"))?;
    Ok(())
}

/// Bring `target_dir` up to date with the server.
pub fn sync_game(
    server: &str,
    target_dir: &Path,
    state_file: &Path,
    progress: Progress,
) -> Result<SyncResult, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .timeout_connect(Duration::from_secs(10))
        .user_agent("pb2-launcher/1.0")
        .build();

    let manifest = fetch_manifest(server, &agent)?;
    fs::create_dir_all(target_dir).map_err(|e| format!("create folder: {e}"))?;

    let state = load_state(state_file);
    let mut next_state: BTreeMap<String, StateEntry> = BTreeMap::new();
    let wanted: HashSet<&str> = manifest.iter().map(|f| f.path.as_str()).collect();

    let bytes_total: u64 = manifest.iter().map(|f| f.size).sum();
    let total = manifest.len();
    let mut bytes: u64 = 0;
    let mut result = SyncResult {
        files: total,
        ..Default::default()
    };

    for (done, file) in manifest.iter().enumerate() {
        let abs = safe_target(target_dir, &file.path)?;

        let mut local_hash: Option<String> = None;
        if let Ok(meta) = fs::metadata(&abs) {
            if meta.is_file() {
                let mt = mtime_ms(&meta);
                let memo = state.get(&file.path);
                local_hash = match memo {
                    Some(m) if m.size == meta.len() && m.mtime_ms == mt => Some(m.sha256.clone()),
                    _ => sha256_file(&abs).ok(),
                };
            }
        }

        let matches = local_hash.as_deref() == Some(file.sha256.as_str());
        if matches {
            result.unchanged += 1;
            bytes += fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
            if let Ok(meta) = fs::metadata(&abs) {
                next_state.insert(
                    file.path.clone(),
                    StateEntry {
                        size: meta.len(),
                        mtime_ms: mtime_ms(&meta),
                        sha256: file.sha256.clone(),
                    },
                );
            }
            continue;
        }

        let mut on_bytes = |n: u64| bytes += n;
        match download_to(&agent, server, &file.path, &abs, &mut on_bytes) {
            Ok(()) => {
                if let Ok(meta) = fs::metadata(&abs) {
                    next_state.insert(
                        file.path.clone(),
                        StateEntry {
                            size: meta.len(),
                            mtime_ms: mtime_ms(&meta),
                            sha256: file.sha256.clone(),
                        },
                    );
                }
                result.downloaded += 1;
                progress(done + 1, total, bytes, bytes_total, &file.path);
            }
            Err(e) => {
                // Keep the previous entry so the next start retries this file
                // rather than believing it is current.
                if let Some(prev) = state.get(&file.path) {
                    next_state.insert(file.path.clone(), prev.clone());
                }
                save_state(state_file, next_state);
                return Err(format!("could not download {e}"));
            }
        }
    }

    // Drop files the server no longer publishes. Everything under target_dir
    // is this launcher's own copy, so nothing else can be caught by this —
    // except the state file, which lives beside the game files.
    let state_abs = normalize(state_file);
    for entry in walkdir::WalkDir::new(target_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path();
        if normalize(abs) == state_abs {
            continue;
        }
        if abs
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(".part"))
            .unwrap_or(false)
        {
            let _ = fs::remove_file(abs);
            continue;
        }
        let rel = match abs.strip_prefix(target_dir) {
            Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if wanted.contains(rel.as_str()) {
            continue;
        }
        if fs::remove_file(abs).is_ok() {
            result.removed += 1;
        }
    }

    save_state(state_file, next_state);
    result.bytes = bytes;
    Ok(result)
}
