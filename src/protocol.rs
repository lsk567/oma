//! Generating the client's protocol from this one.
//!
//! The wire types were written twice — once here as Rust, once in
//! `web/app/lib/protocol.ts` as TypeScript — and only the second copy was ever
//! checked against anything. Nothing made them agree, so they drifted: the
//! daemon answered `stopped` for as long as `RunEnd::Stopped` has existed, and
//! no client had that status written down at all.
//!
//! This emits the TypeScript from the Rust, and the test at the bottom fails
//! when the checked-in file is not what this run produces. The generated file
//! is committed rather than built, because `web.yml`'s Node jobs have no Rust
//! toolchain and must not need one.
//!
//! Vocabularies get a runtime array as well as a type:
//!
//! ```text
//! export const RUN_STATUSES = ["starting", ...] as const;
//! export type RunStatus = (typeof RUN_STATUSES)[number];
//! ```
//!
//! A type alone would not have caught the bug it was written for. TypeScript
//! erases types at build time, and the check that rejected `stopped` was a
//! runtime one — a hand-written list inside `assertRunRecord`. The array is
//! what that check can be written against.

use ts_rs::{Config, TS};

use crate::diagram::{
    wire_name, DiagramAgent, DiagramEdge, DiagramEvent, DiagramEventKind, DiagramInstance,
    DiagramPort, DiagramReaction, DiagramSnapshot, DiagramStatus, DiagramTag, DiagramTimer,
    EdgeKind, ReactionStatus,
};
use crate::serve::{ChatMessage, ChatRole, ProposedDesign, RunRecord, RunStatus};
use crate::topology::PortKind;

/// Where the generated file lives, relative to the repository root.
pub const GENERATED_PATH: &str = "web/app/lib/protocol-generated.ts";

/// One closed set of strings the protocol uses, and the name its runtime array
/// takes in TypeScript.
struct Vocabulary {
    /// The TypeScript array's name, e.g. `RUN_STATUSES`.
    constant: &'static str,
    /// The type declared from it, e.g. `RunStatus`. Given rather than derived
    /// from `constant`: guessing a singular from a plural is a rule that can be
    /// wrong, and it bought nothing.
    ty: &'static str,
    /// Every value, spelled the way serde spells it.
    values: Vec<String>,
    /// The Rust doc comment is the client's too; carried across so the reason
    /// for a vocabulary is not left behind in the runtime.
    doc: &'static str,
}

/// Ask serde for the wire spelling of every variant.
///
/// Listing the variants is unavoidable — Rust cannot enumerate them — but
/// spelling them is not: `wire_name` serialises each one, so a `rename_all`
/// changed here reaches TypeScript without anyone remembering to retype it.
fn vocabulary<T: serde::Serialize>(
    constant: &'static str,
    ty: &'static str,
    doc: &'static str,
    all: &[T],
) -> Vocabulary {
    Vocabulary {
        constant,
        ty,
        values: all
            .iter()
            .map(|value| wire_name(value).expect("a unit variant serialises to a string"))
            .collect(),
        doc,
    }
}

fn vocabularies() -> Vec<Vocabulary> {
    vec![
        vocabulary(
            "RUN_STATUSES",
            "RunStatus",
            "Every status `omar serve` can report a run in.",
            &[
                RunStatus::Starting,
                RunStatus::Running,
                RunStatus::Stopping,
                RunStatus::Completed,
                RunStatus::Stopped,
                RunStatus::Failed,
            ],
        ),
        vocabulary(
            "DIAGRAM_STATUSES",
            "DiagramStatus",
            "Where a drawing stands. `ready` is compiled but never run, which is what a proposal's preview is.",
            &[
                DiagramStatus::Ready,
                DiagramStatus::Running,
                DiagramStatus::Completed,
                DiagramStatus::Failed,
            ],
        ),
        vocabulary(
            "REACTION_STATUSES",
            "ReactionStatus",
            "Where one reaction stands.",
            &[
                ReactionStatus::Idle,
                ReactionStatus::Running,
                ReactionStatus::Completed,
            ],
        ),
        vocabulary(
            "EDGE_KINDS",
            "EdgeKind",
            "What an edge means, which decides how it is drawn.",
            &[EdgeKind::Connection, EdgeKind::Trigger, EdgeKind::Effect],
        ),
        vocabulary(
            "PORT_KINDS",
            "PortKind",
            "What a port is for.",
            &[PortKind::Input, PortKind::Output, PortKind::Action],
        ),
        vocabulary(
            "DIAGRAM_EVENT_KINDS",
            "DiagramEventKind",
            "What happened, as the event stream names it.",
            &[
                DiagramEventKind::RunStarted,
                DiagramEventKind::TagAdvanced,
                DiagramEventKind::ReactionStarted,
                DiagramEventKind::ReactionCompleted,
                DiagramEventKind::RunCompleted,
                DiagramEventKind::RunFailed,
            ],
        ),
        vocabulary(
            "CHAT_ROLES",
            "ChatRole",
            "Who spoke.",
            &[ChatRole::Operator, ChatRole::Assistant],
        ),
    ]
}

/// The whole generated file, as it should be on disk.
pub fn generate() -> String {
    // JSON has one number type, and every integer here crosses the wire inside
    // a JSON number. `bigint` would describe a value no client ever receives.
    let config = Config::new().with_large_int("number");

    let mut out = String::new();
    out.push_str(
        "// Generated from the Rust wire types by `cargo test protocol`. Do not edit.\n\
         //\n\
         // The runtime is the single definition of what the protocol is. Editing this\n\
         // file makes the client disagree with the daemon, which is the failure the\n\
         // generator exists to make impossible.\n\n",
    );

    for vocabulary in vocabularies() {
        let values = vocabulary
            .values
            .iter()
            .map(|value| format!("\"{value}\""))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "/** {} */\nexport const {} = [{}] as const;\nexport type {} = (typeof {})[number];\n\n",
            vocabulary.doc, vocabulary.constant, values, vocabulary.ty, vocabulary.constant
        ));
    }

    // Declared after the vocabularies, because the structs refer to them.
    let mut decls = vec![
        DiagramTag::decl(&config),
        DiagramInstance::decl(&config),
        DiagramAgent::decl(&config),
        DiagramPort::decl(&config),
        DiagramTimer::decl(&config),
        DiagramReaction::decl(&config),
        DiagramEdge::decl(&config),
        DiagramSnapshot::decl(&config),
        DiagramEvent::decl(&config),
        ProposedDesign::decl(&config),
        ChatMessage::decl(&config),
        RunRecord::decl(&config),
    ];
    for decl in &mut decls {
        out.push_str("export ");
        out.push_str(decl);
        out.push_str("\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every vocabulary declares the type its structs are written in terms of.
    ///
    /// A vocabulary left out of `vocabularies()` still reaches the client
    /// through ts-rs, but without its runtime array — the exact shape of the
    /// bug this generator exists to prevent, and invisible without this.
    #[test]
    fn every_vocabulary_the_structs_use_is_declared() {
        let generated = generate();
        for name in [
            "RunStatus",
            "DiagramStatus",
            "ReactionStatus",
            "EdgeKind",
            "PortKind",
            "DiagramEventKind",
            "ChatRole",
        ] {
            assert!(
                generated.contains(&format!("export type {name} = (typeof")),
                "{name} has no runtime array"
            );
            assert!(
                !generated.contains(&format!("export type {name} = \"")),
                "{name} was declared twice: once as a bare union, once with its array"
            );
        }
    }

    /// The names come from serde, so a `rename_all` cannot reach the wire
    /// without reaching the client.
    #[test]
    fn the_values_are_spelled_the_way_serde_spells_them() {
        let generated = generate();
        assert!(generated.contains(
            r#"export const RUN_STATUSES = ["starting", "running", "stopping", "completed", "stopped", "failed"] as const;"#
        ));
        assert!(
            generated.contains(r#"export const CHAT_ROLES = ["operator", "assistant"] as const;"#)
        );
    }

    /// Integers cross the wire inside JSON numbers, so none of them may reach
    /// the client as `bigint` — which is not even the type `JSON.parse` yields.
    #[test]
    fn no_wire_integer_is_a_bigint() {
        assert!(!generate().contains("bigint"));
    }

    /// The checked-in file is what this run produces.
    ///
    /// It is committed rather than built because `web.yml`'s Node jobs have no
    /// Rust toolchain; this is what keeps a committed artefact honest.
    #[test]
    fn the_generated_protocol_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(GENERATED_PATH);
        let generated = generate();
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        if committed == generated {
            return;
        }
        if std::env::var_os("UPDATE_PROTOCOL").is_some() {
            std::fs::write(&path, &generated).expect("failed to write the generated protocol");
            panic!("{GENERATED_PATH} was stale and has been rewritten; commit it");
        }
        panic!(
            "{GENERATED_PATH} is stale.\n\
             The Rust wire types changed and the client's copy did not.\n\
             Regenerate it with:\n\n    UPDATE_PROTOCOL=1 cargo test --bin omar protocol\n"
        );
    }
}
