//! Read-only, path-jailed access to a project folder.
//!
//! Everything an assistant reads goes through here: the path is canonicalized
//! first (so `..` and symlinks cannot escape), then checked against the project
//! root, then against a denylist of things that commonly hold secrets.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

pub const MAX_FILE_BYTES: u64 = 512 * 1024;
pub const MAX_LISTING_ENTRIES: usize = 2000;

/// Directory names never worth traversing and, in `.git`'s case, actively
/// dangerous (`.git/config` can carry credentials in a remote URL).
const DENY_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".gradle",
    "Pods",
    ".terraform",
];

const DENY_NAMES: &[&str] = &[
    ".env",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "credentials",
    "secrets.yaml",
    "secrets.yml",
    "serviceaccount.json",
];

const DENY_EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx", "keystore", "jks", "asc", "gpg"];

fn denied_reason(rel: &Path) -> Option<String> {
    for comp in rel.components() {
        let name = comp.as_os_str().to_string_lossy();
        if DENY_DIRS.contains(&name.as_ref()) {
            return Some(format!("`{name}` is not readable"));
        }
    }
    let name = rel.file_name()?.to_string_lossy().to_string();
    let lower = name.to_ascii_lowercase();

    if DENY_NAMES.contains(&lower.as_str()) || lower.starts_with(".env") {
        return Some(format!("`{name}` may contain secrets"));
    }
    if let Some(ext) = rel.extension().and_then(|e| e.to_str()) {
        if DENY_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
            return Some(format!("`.{ext}` files may contain private keys"));
        }
    }
    None
}

/// Canonicalizes `root` once. Fails loudly if the project folder is gone.
pub fn canonical_root(root: &str) -> Result<PathBuf> {
    std::fs::canonicalize(root)
        .map_err(|e| Error::Invalid(format!("project folder `{root}` is unreadable: {e}")))
}

/// Resolves `requested` (relative, or absolute inside the root) to a real path
/// inside `root`, or explains why not.
pub fn resolve(root: &Path, requested: &str) -> Result<PathBuf> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Ok(root.to_path_buf());
    }
    if requested.contains('\0') {
        return Err(Error::Forbidden("path contains a null byte".into()));
    }

    let candidate = {
        let p = Path::new(requested);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        }
    };

    // Canonicalize the deepest existing ancestor, then re-append the rest, so
    // that a request for a file that does not exist still gets jail-checked.
    let (existing, tail) = deepest_existing(&candidate);
    let real = std::fs::canonicalize(&existing)
        .map_err(|e| Error::NotFound(format!("{}: {e}", existing.display())))?;
    // Joining an empty tail would append a trailing separator, which makes
    // `metadata` fail with NotADirectory on a plain file.
    let real = if tail.as_os_str().is_empty() {
        real
    } else {
        real.join(&tail)
    };

    let rel = real
        .strip_prefix(root)
        .map_err(|_| Error::Forbidden(format!("`{requested}` is outside the project folder")))?;

    if let Some(reason) = denied_reason(rel) {
        return Err(Error::Forbidden(reason));
    }
    Ok(real)
}

fn deepest_existing(path: &Path) -> (PathBuf, PathBuf) {
    let mut existing = path.to_path_buf();
    let mut tail = PathBuf::new();
    loop {
        if existing.exists() {
            return (existing, tail);
        }
        let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
            return (PathBuf::from("/"), tail);
        };
        tail = if tail.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            Path::new(&name).join(&tail)
        };
        if !existing.pop() {
            return (PathBuf::from("/"), tail);
        }
    }
}

pub struct FileSlice {
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub total_lines: i64,
    pub content: String,
    pub truncated: bool,
}

pub fn read_slice(
    root: &Path,
    requested: &str,
    start: Option<i64>,
    end: Option<i64>,
) -> Result<FileSlice> {
    let real = resolve(root, requested)?;
    let meta = std::fs::metadata(&real)?;
    if meta.is_dir() {
        return Err(Error::Invalid(format!("{requested} is a directory")));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(Error::Limit(format!(
            "{requested} is {} bytes; the limit is {MAX_FILE_BYTES}. Request a line range.",
            meta.len()
        )));
    }

    let raw = std::fs::read(&real)?;
    if raw.contains(&0u8) {
        return Err(Error::Invalid(format!("{requested} looks binary")));
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len() as i64;

    let start = start.unwrap_or(1).max(1);
    let end = end.unwrap_or(total).min(total).max(start);
    let slice = if total == 0 {
        String::new()
    } else {
        lines[(start - 1).min(total) as usize..end.min(total) as usize].join("\n")
    };

    let rel = real
        .strip_prefix(root)
        .unwrap_or(&real)
        .to_string_lossy()
        .to_string();

    Ok(FileSlice {
        path: rel,
        start_line: start,
        end_line: end,
        total_lines: total,
        content: slice,
        truncated: start > 1 || end < total,
    })
}

/// Shallow-to-deep directory listing, skipping denied directories entirely.
pub fn list_dir(root: &Path, requested: &str, depth: usize) -> Result<Vec<String>> {
    let base = resolve(root, requested)?;
    let mut out = Vec::new();
    let mut stack = vec![(base.clone(), 0usize)];

    while let Some((dir, d)) = stack.pop() {
        if out.len() >= MAX_LISTING_ENTRIES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            if denied_reason(rel).is_some() {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push(format!("{}{}", rel.to_string_lossy(), if is_dir { "/" } else { "" }));
            if is_dir && d + 1 < depth {
                stack.push((path, d + 1));
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rivendell-jail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "a\nb\nc\nd\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/config"), "[remote]").unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn allows_paths_inside_root() {
        let root = tmp();
        assert!(resolve(&root, "src/main.rs").is_ok());
    }

    #[test]
    fn blocks_traversal() {
        let root = tmp();
        assert!(matches!(
            resolve(&root, "../../../etc/passwd"),
            Err(Error::Forbidden(_)) | Err(Error::NotFound(_))
        ));
        assert!(resolve(&root, "/etc/passwd").is_err());
    }

    #[test]
    fn blocks_secrets_and_git() {
        let root = tmp();
        assert!(matches!(resolve(&root, ".env"), Err(Error::Forbidden(_))));
        assert!(matches!(resolve(&root, ".env.local"), Err(Error::Forbidden(_))));
        assert!(matches!(resolve(&root, ".git/config"), Err(Error::Forbidden(_))));
        assert!(matches!(resolve(&root, "server.pem"), Err(Error::Forbidden(_))));
    }

    #[test]
    fn reads_line_ranges() {
        let root = tmp();
        let s = read_slice(&root, "src/main.rs", Some(2), Some(3)).unwrap();
        assert_eq!(s.content, "b\nc");
        assert_eq!(s.total_lines, 4);
        assert!(s.truncated);
    }

    #[test]
    fn listing_hides_denied_dirs() {
        let root = tmp();
        let items = list_dir(&root, "", 3).unwrap();
        assert!(items.iter().any(|i| i == "src/"));
        assert!(!items.iter().any(|i| i.starts_with(".git")));
        assert!(!items.iter().any(|i| i.starts_with(".env")));
    }
}
