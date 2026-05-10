#![allow(
    missing_docs,
    reason = "Integration test names describe the gap-doc walker contract under test."
)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use eyre::{Result, eyre};

const GAP_DOC: &str = include_str!("../../../../docs/reference/closing-the-zaino-surface-gap.md");

const SCAN_DIRS: &[&str] = &["crates", "services"];

#[test]
fn every_tag_references_a_consistent_row() -> Result<()> {
    let doc = parse_gap_doc(GAP_DOC)?;
    let tags = collect_tags()?;

    let mut errors = Vec::new();
    for tag in &tags {
        match tag.kind {
            TagKind::Gap => {
                if !doc.gaps.contains(&tag.id) {
                    errors.push(format!(
                        "gap: {} at {} references a row that does not exist in the gap doc; \
                         add the row or remove the tag",
                        tag.id, tag.site
                    ));
                } else if doc.closed_gaps.contains(&tag.id) {
                    errors.push(format!(
                        "gap: {} at {} references a row marked '✓ closed'; \
                         change the tag to 'closes: {}' or remove it",
                        tag.id, tag.site, tag.id
                    ));
                }
            }
            TagKind::Closes => {
                if !doc.gaps.contains(&tag.id) {
                    errors.push(format!(
                        "closes: {} at {} references a row that does not exist in the gap doc; \
                         add the row or remove the tag",
                        tag.id, tag.site
                    ));
                } else if !doc.closed_gaps.contains(&tag.id) {
                    errors.push(format!(
                        "closes: {} at {} references a row that is not marked '✓ closed' in the gap doc; \
                         mark the row closed in docs/reference/closing-the-zaino-surface-gap.md first",
                        tag.id, tag.site
                    ));
                }
            }
            TagKind::Refuses => {
                if !doc.anti_patterns.contains(&tag.id) {
                    errors.push(format!(
                        "refuses: {} at {} references an anti-pattern that does not exist in the gap doc; \
                         add the row or remove the tag",
                        tag.id, tag.site
                    ));
                }
            }
        }
    }
    if !errors.is_empty() {
        return Err(eyre!(
            "gap-doc tag drift detected ({} issues):\n  {}",
            errors.len(),
            errors.join("\n  ")
        ));
    }
    Ok(())
}

#[test]
fn every_closed_gap_row_has_a_closes_tag() -> Result<()> {
    let doc = parse_gap_doc(GAP_DOC)?;
    let tags = collect_tags()?;

    let closed_with_tags: BTreeSet<&str> = tags
        .iter()
        .filter(|tag| matches!(tag.kind, TagKind::Closes))
        .map(|tag| tag.id.as_str())
        .collect();

    let mut missing: Vec<&str> = doc
        .closed_gaps
        .iter()
        .filter(|id| !closed_with_tags.contains(id.as_str()))
        .map(String::as_str)
        .collect();
    missing.sort_unstable();
    if !missing.is_empty() {
        return Err(eyre!(
            "gap rows marked '✓ closed' have no `/// closes: G{{N}}` tag in code:\n  {}\n\
             Add the tag to the public type, trait method, or RPC handler that closes the gap.",
            missing.join(", ")
        ));
    }
    Ok(())
}

struct ParsedDoc {
    gaps: BTreeSet<String>,
    closed_gaps: BTreeSet<String>,
    anti_patterns: BTreeSet<String>,
}

fn parse_gap_doc(doc: &str) -> Result<ParsedDoc> {
    let mut gaps = BTreeSet::new();
    let mut closed_gaps = BTreeSet::new();
    let mut anti_patterns = BTreeSet::new();

    for line in doc.lines() {
        if let Some(rest) = line.strip_prefix("### G") {
            if let Some(id) = parse_heading_id(rest, 'G') {
                gaps.insert(id.clone());
                if line.contains("✓ closed") {
                    closed_gaps.insert(id);
                }
            }
        } else if let Some(rest) = line.strip_prefix("### A")
            && let Some(id) = parse_heading_id(rest, 'A')
        {
            anti_patterns.insert(id);
        }
    }

    if gaps.is_empty() || anti_patterns.is_empty() {
        return Err(eyre!(
            "gap doc parser found {} gap rows and {} anti-pattern rows; \
             the parser may be out of sync with the doc structure",
            gaps.len(),
            anti_patterns.len()
        ));
    }

    Ok(ParsedDoc {
        gaps,
        closed_gaps,
        anti_patterns,
    })
}

fn parse_heading_id(rest_after_prefix: &str, prefix: char) -> Option<String> {
    let id_end = rest_after_prefix.find('.')?;
    let number = &rest_after_prefix[..id_end];
    if number.chars().all(|character| character.is_ascii_digit()) {
        Some(format!("{prefix}{number}"))
    } else {
        None
    }
}

#[derive(Debug)]
struct Tag {
    kind: TagKind,
    id: String,
    site: String,
}

#[derive(Debug)]
enum TagKind {
    Gap,
    Closes,
    Refuses,
}

fn collect_tags() -> Result<Vec<Tag>> {
    let workspace_root = workspace_root()?;
    let mut tags = Vec::new();
    for directory in SCAN_DIRS {
        walk(&workspace_root.join(directory), &workspace_root, &mut tags)?;
    }
    Ok(tags)
}

fn walk(directory: &Path, workspace_root: &Path, tags: &mut Vec<Tag>) -> Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if file_name == "target" || file_name == ".git" || file_name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk(&path, workspace_root, tags)?;
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            scan_file(&path, workspace_root, tags)?;
        }
    }
    Ok(())
}

fn scan_file(path: &Path, workspace_root: &Path, tags: &mut Vec<Tag>) -> Result<()> {
    let contents = fs::read_to_string(path)?;
    let display_path = path.strip_prefix(workspace_root).map_or_else(
        |_| path.display().to_string(),
        |relative| relative.display().to_string(),
    );
    for (line_index, line) in contents.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some((kind, id)) = parse_tag(trimmed) {
            tags.push(Tag {
                kind,
                id,
                site: format!("{display_path}:{}", line_index + 1),
            });
        }
    }
    Ok(())
}

fn parse_tag(line: &str) -> Option<(TagKind, String)> {
    let body = line
        .strip_prefix("///")
        .or_else(|| line.strip_prefix("//!"))?
        .trim_start();
    let (kind, rest) = if let Some(rest) = body.strip_prefix("gap:") {
        (TagKind::Gap, rest)
    } else if let Some(rest) = body.strip_prefix("closes:") {
        (TagKind::Closes, rest)
    } else if let Some(rest) = body.strip_prefix("refuses:") {
        (TagKind::Refuses, rest)
    } else {
        return None;
    };
    let id = rest
        .split_whitespace()
        .next()?
        .trim_end_matches(|character: char| !character.is_ascii_alphanumeric())
        .to_owned();
    if id.is_empty() {
        return None;
    }
    Some((kind, id))
}

fn workspace_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            eyre!(
                "could not determine workspace root from manifest dir {}",
                manifest.display()
            )
        })
}
