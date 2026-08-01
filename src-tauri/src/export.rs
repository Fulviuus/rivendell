//! Resolved threads are written into the repo as decision records, so the
//! reasoning outlives the app and future agents can read it as context.

use crate::error::Result;
use crate::models::ThreadDetail;

pub fn slug(title: &str) -> String {
    let s: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let s = s
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    s.chars().take(60).collect::<String>().trim_matches('-').to_string()
}

pub fn render(t: &ThreadDetail) -> String {
    let s = &t.summary;
    let mut out = String::new();

    out.push_str("---\n");
    out.push_str(&format!("thread: {}\n", s.id));
    out.push_str(&format!("title: {:?}\n", s.title));
    out.push_str(&format!("room: {}\n", s.room_name));
    out.push_str(&format!("tag: {}\n", s.tag));
    out.push_str(&format!("status: {}\n", s.status));
    out.push_str(&format!("opened_by: {}\n", s.author_name));
    out.push_str(&format!("created: {}\n", s.created_at));
    if let Some(r) = &s.resolved_at {
        out.push_str(&format!("resolved: {r}\n"));
    }
    if let Some(g) = &s.git_ref {
        out.push_str(&format!("git_ref: {g}\n"));
        if t.git_dirty {
            out.push_str("git_dirty: true\n");
        }
    }
    if s.cost_usd > 0.0 {
        out.push_str(&format!("cost_usd: {:.4}\n", s.cost_usd));
    }
    out.push_str("---\n\n");

    out.push_str(&format!("# {}\n\n", s.title));

    if let Some(res) = &t.resolution_summary {
        out.push_str("## Resolution\n\n");
        out.push_str(res.trim());
        out.push_str("\n\n");
    }

    out.push_str(&format!("## Topic — {} ({})\n\n", s.author_name, s.tag));
    out.push_str(t.body.trim());
    out.push_str("\n\n");

    if !t.context.is_empty() {
        out.push_str("## Context as posted\n\n");
        for c in &t.context {
            let header = match (&c.path, c.start_line, c.end_line) {
                (Some(p), Some(a), Some(b)) => format!("{p}:{a}-{b}"),
                (Some(p), _, _) => p.clone(),
                _ => c.kind.clone(),
            };
            out.push_str(&format!("### {header}\n\n"));
            let lang = if c.kind == "diff" {
                "diff"
            } else {
                c.path
                    .as_deref()
                    .and_then(|p| p.rsplit('.').next())
                    .unwrap_or("")
            };
            out.push_str(&format!("```{lang}\n"));
            out.push_str(c.content.trim_end());
            out.push_str("\n```\n\n");
        }
    }

    out.push_str("## Discussion\n\n");
    if t.messages.is_empty() {
        out.push_str("_No replies._\n\n");
    }
    for m in &t.messages {
        let mut head = format!("### {} ({})", m.agent_name, m.agent_role);
        if let Some(v) = &m.verdict {
            head.push_str(&format!(" — **{v}**"));
        }
        if let Some(sev) = &m.severity {
            head.push_str(&format!(" · {sev}"));
        }
        if m.edited_at.is_some() {
            head.push_str(" · _edited_");
        }
        out.push_str(&head);
        out.push_str(&format!("\n\n<sub>{}</sub>\n\n", m.created_at));
        out.push_str(m.body.trim());
        out.push_str("\n\n");
        if let Some(refs) = m.refs.as_array() {
            if !refs.is_empty() {
                out.push_str("References:\n");
                for r in refs {
                    let path = r.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let line = r.get("line").and_then(|v| v.as_i64());
                    let note = r.get("note").and_then(|v| v.as_str()).unwrap_or("");
                    match line {
                        Some(l) => out.push_str(&format!("- `{path}:{l}` {note}\n")),
                        None => out.push_str(&format!("- `{path}` {note}\n")),
                    }
                }
                out.push('\n');
            }
        }
    }

    out.push_str("---\n\n<sub>Recorded by Rivendell.</sub>\n");
    out
}

/// Writes `<project>/.rivendell/threads/<id>-<slug>.md`, returning the path.
pub fn write_thread(project_folder: &str, t: &ThreadDetail) -> Result<String> {
    let dir = std::path::Path::new(project_folder)
        .join(".rivendell")
        .join("threads");
    std::fs::create_dir_all(&dir)?;

    let name = format!("{:04}-{}.md", t.summary.id, slug(&t.summary.title));
    let path = dir.join(name);
    std::fs::write(&path, render(t))?;
    Ok(path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_filename_safe() {
        assert_eq!(slug("Fix the OAuth / token refresh bug!"), "fix-the-oauth-token-refresh-bug");
        assert_eq!(slug("   "), "");
        assert!(slug(&"x".repeat(200)).len() <= 60);
    }
}
