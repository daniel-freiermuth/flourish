#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The UI has to be able to render every finding type and status the daemon emits.
//!
//! `FindingType` is serialized onto the wire and the Svelte UI switches on
//! those strings to pick a label and an icon. Nothing connects the two, so
//! adding a variant silently degrades the UI: the type pill falls back to
//! the raw wire name and the icon to a question mark. That is exactly what
//! happened when `standards` was added — five cases, six variants.
//!
//! This lives on the Rust side deliberately. The natural home looks like a
//! vitest case, but it needs to read a file, and the frontend's tsconfig
//! has no node types — the first attempt passed locally and failed
//! `svelte-check` in CI. Here, reading a sibling file is ordinary.

use std::path::Path;

use hunter::domain::{
    FindingJobKind, FindingStatus, FindingType, HoldReason, ReleaseAction, RepoJobKind,
};
use ts_rs::TS;

fn format_ts() -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hunter/ui-svelte/src/lib/format.ts");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// The body of `export function {name}`, up to the next export.
///
/// Every check below is scoped to one helper's body. Searching a wider span
/// -- "everything before `typeEmoji`", "everything after it" -- lets a case
/// in a neighbouring function satisfy the check for the one that lost it,
/// and the test would then pass on exactly the regression it exists for.
fn helper_body<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src
        .find(&format!("export function {name}("))
        .unwrap_or_else(|| panic!("format.ts must define {name}"));
    let body = &src[start..];
    let end = body[1..].find("\nexport ").map_or(body.len(), |i| i + 1);
    &body[..end]
}

/// Every wire name has an explicit case in both helpers.
#[test]
fn the_ui_renders_every_finding_type() {
    let src = format_ts();
    let mut missing = Vec::new();
    for helper in ["typeLabel", "typeEmoji"] {
        let body = helper_body(&src, helper);
        for t in FindingType::ALL {
            if !body.contains(&format!("case \"{}\":", t.as_str())) {
                missing.push(format!("{helper} has no case for {:?}", t.as_str()));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "hunter/ui-svelte/src/lib/format.ts is missing finding types: {missing:#?}"
    );
}

/// The fallback stays reachable. `FindingOut.type` is the generated
/// `FindingType` union, but that only describes the daemon this UI was
/// built against: an older UI meeting a newer daemon receives a type it
/// has never heard of and must degrade rather than break.
#[test]
fn the_ui_keeps_a_fallback_for_unknown_types() {
    let src = format_ts();
    for helper in ["typeLabel", "typeEmoji"] {
        assert!(
            helper_body(&src, helper).contains("default:"),
            "{helper} must keep a default arm for a type this build has not heard of"
        );
    }
}

/// The generated `JobKind` union spells the wire strings `as_str` sends.
///
/// `JobKind` serializes through a hand-written `as_str`, while its
/// TypeScript type comes from `#[ts(rename_all = "snake_case")]` on the
/// two kind enums. Every other enum derives both from one serde attribute;
/// here the two spellings are independent, so a renamed wire string would
/// leave the UI typed against a value the daemon no longer sends.
#[test]
fn the_generated_job_kinds_are_the_wire_strings() {
    let cfg = ts_rs::Config::new();
    let generated: Vec<String> = [RepoJobKind::inline(&cfg), FindingJobKind::inline(&cfg)]
        .iter()
        .flat_map(|union| union.split(" | ").map(str::to_owned).collect::<Vec<_>>())
        .collect();
    let wire: Vec<String> = RepoJobKind::ALL
        .iter()
        .map(|k| k.as_str())
        .chain(FindingJobKind::ALL.iter().map(|k| k.as_str()))
        .map(|s| format!("\"{s}\""))
        .collect();
    assert_eq!(generated, wire);
}

/// Every finding status gets its own badge colour.
///
/// Same failure as the types above, for statuses: a status the switch does
/// not name renders with the neutral fallback, indistinguishable from any
/// other unknown value. `closed` and `superseded` were added for closed
/// PRs, which are mostly superseded work rather than rejections, and a
/// badge that shows them like nothing in particular hides exactly that.
#[test]
fn the_ui_badges_every_finding_status() {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../hunter/ui-svelte/src/components/FindingCard.svelte");
    let src =
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
    let start = src
        .find("function statusBadgeClass(")
        .expect("FindingCard.svelte must define statusBadgeClass");
    let body = &src[start..];
    let body = &body[..body[1..]
        .find("\n  function ")
        .map_or(body.len(), |i| i + 1)];
    let missing: Vec<&str> = FindingStatus::ALL
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !body.contains(&format!("case \"{s}\":")))
        .collect();
    assert!(
        missing.is_empty(),
        "statusBadgeClass in FindingCard.svelte has no case for {missing:?}"
    );
}

/// Every hold reason and release action has its own label, and an unknown
/// one still renders.
///
/// The Needs-you cards print why work is held and one button per action
/// the daemon offers (`HoldOut.actions`). A reason or action the switch
/// does not name shows its raw wire string: legible, but it is the same
/// silent degradation the finding types had.
#[test]
fn the_ui_labels_every_hold_reason_and_release_action() {
    let src = format_ts();
    let mut missing = Vec::new();
    for (helper, wire) in [
        (
            "holdReasonLabel",
            HoldReason::ALL.map(HoldReason::as_str).to_vec(),
        ),
        (
            "releaseActionLabel",
            ReleaseAction::ALL.map(ReleaseAction::as_str).to_vec(),
        ),
    ] {
        let body = helper_body(&src, helper);
        assert!(body.contains("default:"), "{helper} needs a fallback");
        for w in wire {
            if !body.contains(&format!("case \"{w}\":")) {
                missing.push(format!("{helper} has no case for {w:?}"));
            }
        }
    }
    assert!(missing.is_empty(), "format.ts: {missing:#?}");
}
