//! Thin shell-out wrapper around `git`.
//!
//! A thread pins the commit it was posted against so a review stays
//! reproducible after the coder keeps committing.

use crate::error::{Error, Result};
use std::path::Path;
use std::process::Command;

pub const MAX_DIFF_BYTES: usize = 400 * 1024;

fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| Error::Invalid(format!("could not run git: {e}")))?;
    if !out.status.success() {
        return Err(Error::Invalid(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

pub fn is_repo(dir: &Path) -> bool {
    run(dir, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s == "true")
        .unwrap_or(false)
}

pub fn head(dir: &Path) -> Option<String> {
    run(dir, &["rev-parse", "HEAD"]).ok()
}

pub fn branch(dir: &Path) -> Option<String> {
    run(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()
}

pub fn remote(dir: &Path) -> Option<String> {
    run(dir, &["remote", "get-url", "origin"]).ok()
}

pub fn is_dirty(dir: &Path) -> bool {
    run(dir, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Working-tree diff, or the diff against `base` when given.
pub fn diff(dir: &Path, base: Option<&str>, path: Option<&str>) -> Result<String> {
    if !is_repo(dir) {
        return Err(Error::Invalid("this project folder is not a git repo".into()));
    }
    let mut args: Vec<String> = vec!["diff".into(), "--no-color".into(), "--stat=200".into()];
    if let Some(b) = base {
        validate_rev(b)?;
        args.push(b.to_string());
    }
    if let Some(p) = path {
        args.push("--".into());
        args.push(p.to_string());
    }
    let stat = run(dir, &args.iter().map(String::as_str).collect::<Vec<_>>())?;

    let mut args: Vec<String> = vec!["diff".into(), "--no-color".into()];
    if let Some(b) = base {
        args.push(b.to_string());
    }
    if let Some(p) = path {
        args.push("--".into());
        args.push(p.to_string());
    }
    let body = run(dir, &args.iter().map(String::as_str).collect::<Vec<_>>())?;

    let mut combined = if stat.is_empty() {
        body
    } else {
        format!("{stat}\n\n{body}")
    };
    if combined.len() > MAX_DIFF_BYTES {
        combined.truncate(MAX_DIFF_BYTES);
        combined.push_str("\n\n… diff truncated; ask for a specific path.");
    }
    Ok(combined)
}

/// Rejects anything that is not plausibly a revision, so a rev can never turn
/// into an extra git argument.
fn validate_rev(rev: &str) -> Result<()> {
    if rev.is_empty() || rev.len() > 200 {
        return Err(Error::Invalid("bad git revision".into()));
    }
    if rev.starts_with('-') {
        return Err(Error::Invalid("git revision may not start with `-`".into()));
    }
    if !rev
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._/-~^@{}".contains(c))
    {
        return Err(Error::Invalid(format!("bad git revision `{rev}`")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_flag_injection() {
        assert!(validate_rev("--upload-pack=evil").is_err());
        assert!(validate_rev("HEAD; rm -rf /").is_err());
        assert!(validate_rev("HEAD~3").is_ok());
        assert!(validate_rev("main").is_ok());
        assert!(validate_rev("origin/feature/x").is_ok());
    }
}
