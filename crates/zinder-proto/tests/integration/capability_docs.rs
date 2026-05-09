#![allow(
    missing_docs,
    reason = "Integration test names describe the capability-docs contract under test."
)]

use std::collections::BTreeSet;

use eyre::{Result, eyre};
use zinder_proto::ZINDER_CAPABILITIES;

const PUBLIC_INTERFACES_DOC: &str =
    include_str!("../../../../docs/architecture/public-interfaces.md");
const TESTING_RUNBOOK_DOC: &str = include_str!("../../../../docs/runbooks/testing.md");

const PUBLIC_INTERFACES_START: &str = "<!-- capability-list:public-interfaces:start -->";
const PUBLIC_INTERFACES_END: &str = "<!-- capability-list:public-interfaces:end -->";
const TESTING_RUNBOOK_START: &str = "<!-- capability-list:testing-runbook:start -->";
const TESTING_RUNBOOK_END: &str = "<!-- capability-list:testing-runbook:end -->";

#[test]
fn public_interfaces_capability_list_mirrors_zinder_capabilities() -> Result<()> {
    assert_capability_list_matches(
        "docs/architecture/public-interfaces.md",
        PUBLIC_INTERFACES_DOC,
        PUBLIC_INTERFACES_START,
        PUBLIC_INTERFACES_END,
    )
}

#[test]
fn testing_runbook_capability_list_mirrors_zinder_capabilities() -> Result<()> {
    assert_capability_list_matches(
        "docs/runbooks/testing.md",
        TESTING_RUNBOOK_DOC,
        TESTING_RUNBOOK_START,
        TESTING_RUNBOOK_END,
    )
}

fn assert_capability_list_matches(
    doc_path: &str,
    doc: &str,
    start_marker: &str,
    end_marker: &str,
) -> Result<()> {
    let parsed = parse_capability_list(doc, start_marker, end_marker)?;
    let expected: BTreeSet<String> = ZINDER_CAPABILITIES
        .iter()
        .copied()
        .map(String::from)
        .collect();

    let missing_from_doc: Vec<&String> = expected.difference(&parsed).collect();
    let extra_in_doc: Vec<&String> = parsed.difference(&expected).collect();

    assert!(
        missing_from_doc.is_empty() && extra_in_doc.is_empty(),
        "{doc_path} capability list has drifted from ZINDER_CAPABILITIES.\n\
         Missing from doc: {missing_from_doc:?}\n\
         Extra in doc:    {extra_in_doc:?}\n\
         Update the doc list (or update ZINDER_CAPABILITIES if a capability \
         was added/removed) so the two stay in sync."
    );
    Ok(())
}

fn parse_capability_list(
    doc: &str,
    start_marker: &str,
    end_marker: &str,
) -> Result<BTreeSet<String>> {
    let after_start = doc
        .split_once(start_marker)
        .ok_or_else(|| eyre!("missing start marker {start_marker}"))?
        .1;
    let block = after_start
        .split_once(end_marker)
        .ok_or_else(|| eyre!("missing end marker {end_marker}"))?
        .0;
    Ok(block.lines().filter_map(parse_capability_line).collect())
}

fn parse_capability_line(line: &str) -> Option<String> {
    let trimmed = line.trim().trim_start_matches("- ").trim_matches('`');
    if trimmed.is_empty() || trimmed.starts_with("```") {
        return None;
    }
    if !trimmed.contains('.') {
        return None;
    }
    let (_, suffix) = trimmed.rsplit_once("_v")?;
    if suffix.chars().all(|character| character.is_ascii_digit()) {
        Some(trimmed.to_owned())
    } else {
        None
    }
}
