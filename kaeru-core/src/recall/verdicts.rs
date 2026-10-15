//! Which nodes the graph itself says are no longer current (#92).
//!
//! Some edge types point at a node to **cancel** it, not to support it. Every
//! rule that weighs a node by how many live nodes point at it has to know the
//! difference, or the edge that says "this is obsolete" becomes the edge that
//! keeps it loaded — which is exactly what happened to three `core` nodes of
//! one live initiative, superseded weeks earlier and injected into every
//! session since.
//!
//! This module is the one place that knows which types those are, so the
//! hygiene pass and `awake` cannot drift apart about it.

use std::collections::BTreeMap;

use cozo::{DataValue, ScriptMutability};

use crate::errors::Result;
use crate::graph::NodeId;
use crate::store::Store;

/// Edge types that cancel what they point at instead of supporting it.
///
/// `consolidated_to` is deliberately absent: it runs old → new, so the
/// inbound one sits on the *summary*, and treating it as a cancellation
/// would penalise the node that survived rather than the one consolidated
/// away.
pub const CANCELLING: [&str; 3] = ["supersedes", "contradicts", "falsifies"];

/// The cancellations that are a **verdict** — something replaced this node,
/// or refuted it — as opposed to `contradicts`, which is a doubt raised by
/// `flag` and not yet settled.
pub const VERDICT: [&str; 2] = ["supersedes", "falsifies"];

/// A live node's standing objection: what was written against it, and by
/// whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// `supersedes` or `falsifies` — see [`VERDICT`].
    pub edge_type: String,
    /// The name of the node that carries the verdict.
    pub peer: String,
}

impl Verdict {
    /// How a reader should be told, e.g. ``superseded by `state-evening` ``.
    pub fn phrase(&self) -> String {
        let verb = match self.edge_type.as_str() {
            "supersedes" => "superseded",
            "falsifies" => "refuted",
            other => other,
        };
        format!("{verb} by `{}`", self.peer)
    }
}

/// Every node that a **live** node has passed a verdict on, at NOW.
///
/// Cross-initiative on purpose: the node that replaces a fact is often
/// captured in the scope the work moved to, and a supersession that only
/// counts inside one initiative is a supersession an agent can walk around.
/// A verdict held by a retracted node does not count — the same rule the
/// inbound-edge count has always used.
pub fn verdicts_against(store: &Store) -> Result<BTreeMap<NodeId, Verdict>> {
    let types = format!(
        "[{}]",
        VERDICT
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let rows = store.db_ref().run_script(
        &format!(
            r#"
            ?[dst, edge_type, name] := *edge{{src, dst, edge_type @ 'NOW'}},
                                       is_in(edge_type, {types}),
                                       *node{{id: src, name @ 'NOW'}}
            "#
        ),
        BTreeMap::new(),
        ScriptMutability::Immutable,
    )?;

    let mut out: BTreeMap<NodeId, Verdict> = BTreeMap::new();
    for r in &rows.rows {
        let (Some(dst), Some(edge_type), Some(name)) = (
            r.first().and_then(|v| v.get_str()),
            r.get(1).and_then(|v| v.get_str()),
            r.get(2).and_then(|v| v.get_str()),
        ) else {
            continue;
        };
        out.entry(dst.to_string()).or_insert_with(|| Verdict {
            edge_type: edge_type.to_string(),
            peer: name.to_string(),
        });
    }
    Ok(out)
}

/// What the graph says about one node in a listing — read at the moment an
/// agent is about to rely on it (#114).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Standing {
    /// A verdict passed on the node: something superseded or refuted it.
    pub verdict: Option<Verdict>,
    /// The node that raised an open doubt against it (`contradicts`, written
    /// by `flag`) — not settled, so not a verdict, but not to be trusted
    /// blindly either.
    pub disputed_by: Option<String>,
    /// How many edges `neighbours` lists for the node in the active scope.
    pub links: usize,
}

impl Standing {
    /// `superseded by `x`` / `refuted by `x`` / `disputed by `x``, or `None`
    /// for a node the graph has said nothing against.
    pub fn warning(&self) -> Option<String> {
        match (&self.verdict, &self.disputed_by) {
            (Some(v), _) => Some(v.phrase()),
            (None, Some(peer)) => Some(format!("disputed by `{peer}`")),
            (None, None) => None,
        }
    }
}

/// The [`Standing`] of each of `ids`, in one query.
///
/// A search hit is the one listing where an agent decides to trust a node,
/// and it used to carry nothing but an excerpt: the edge that said "this was
/// replaced" sat one `neighbours` call away, and agents do not make that call
/// — graph verbs follow fewer than one search in twenty. So the hit says it
/// itself, the way `awake` already marks a superseded node.
///
/// Verdicts and doubts are read across initiatives, like
/// [`verdicts_against`]: the node that replaces a fact is often captured in
/// the scope the work moved to. `links` follows the active scope instead,
/// because it predicts what `neighbours` will show there. Retracted peers,
/// self-loops, cloud soft links and `audit_event` nodes count for nothing, as
/// in `neighbours`.
pub fn standing(store: &Store, ids: &[NodeId]) -> Result<BTreeMap<NodeId, Standing>> {
    let mut out: BTreeMap<NodeId, Standing> = ids
        .iter()
        .map(|id| (id.clone(), Standing::default()))
        .collect();
    if ids.is_empty() {
        return Ok(out);
    }

    // The seeds go in as parameters, one per row of an inline relation, so no
    // id is ever spliced into the script text.
    let mut params: BTreeMap<String, DataValue> = BTreeMap::new();
    let mut seed_rows = Vec::with_capacity(ids.len());
    for (i, id) in ids.iter().enumerate() {
        params.insert(format!("s{i}"), DataValue::Str(id.clone().into()));
        seed_rows.push(format!("[$s{i}]"));
    }
    // In scope = attached to the active initiative. Unscoped, every live peer
    // is in scope, so there is nothing to join — and a `scope` relation built
    // from `node` would scan the whole vault to say so.
    let peers = match store.current_initiative() {
        Some(init) => {
            params.insert("init".to_string(), DataValue::Str(init.into()));
            r#"
        scope[id] := *node_initiative{initiative, node_id: id}, initiative = $init
        ?[seed, name, edge_type, inbound, in_scope] := link[seed, peer, edge_type, inbound],
            peer != seed, *node{id: peer, name, type @ 'NOW'}, type != 'audit_event',
            scope[peer], in_scope = 1
        ?[seed, name, edge_type, inbound, in_scope] := link[seed, peer, edge_type, inbound],
            peer != seed, *node{id: peer, name, type @ 'NOW'}, type != 'audit_event',
            not scope[peer], in_scope = 0"#
        }
        None => {
            r#"
        ?[seed, name, edge_type, inbound, in_scope] := link[seed, peer, edge_type, inbound],
            peer != seed, *node{id: peer, name, type @ 'NOW'}, type != 'audit_event',
            in_scope = 1"#
        }
    };
    // Inbound edges go through the `edge:by_dst` index and then back to the
    // base relation with the whole key bound, for its NOW validity. Binding
    // only `dst` on the base atom trips a range panic in Cozo's BTreeMap
    // search, and leaving it unbound scans every edge in the vault.
    let script = format!(
        r#"
        seeds[id] <- [{seeds}]
        link[seed, peer, edge_type, inbound] := seeds[seed],
            *edge{{src: seed, dst: peer, edge_type, dst_store @ 'NOW'}},
            dst_store = 'local', inbound = 0
        into[seed, peer, edge_type] := seeds[seed], *edge:by_dst{{dst: seed, src: peer, edge_type}}
        link[seed, peer, edge_type, inbound] := into[seed, peer, edge_type],
            *edge{{src: peer, dst: seed, edge_type, dst_store @ 'NOW'}},
            dst_store = 'local', inbound = 1
        {peers}
        "#,
        seeds = seed_rows.join(", "),
    );
    let rows = store
        .db_ref()
        .run_script(&script, params, ScriptMutability::Immutable)?;

    for r in &rows.rows {
        let (Some(seed), Some(peer), Some(edge_type), Some(inbound), Some(in_scope)) = (
            r.first().and_then(|v| v.get_str()),
            r.get(1).and_then(|v| v.get_str()),
            r.get(2).and_then(|v| v.get_str()),
            r.get(3).and_then(|v| v.get_int()).map(|n| n == 1),
            r.get(4).and_then(|v| v.get_int()).map(|n| n == 1),
        ) else {
            continue;
        };
        let Some(entry) = out.get_mut(seed) else {
            continue;
        };
        if in_scope {
            entry.links += 1;
        }
        if !inbound {
            continue;
        }
        if VERDICT.contains(&edge_type) {
            entry.verdict.get_or_insert_with(|| Verdict {
                edge_type: edge_type.to_string(),
                peer: peer.to_string(),
            });
        } else if edge_type == "contradicts" {
            entry.disputed_by.get_or_insert_with(|| peer.to_string());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::slice::from_ref;

    use super::{standing, verdicts_against};
    use crate::graph::{EdgeType, NodeId, Significance};
    use crate::store::Store;
    use crate::{EpisodeKind, write_episode};

    fn episode(store: &Store, name: &str) -> NodeId {
        write_episode(
            store,
            EpisodeKind::Observation,
            Significance::Medium,
            name,
            "body",
        )
        .expect("write")
    }

    #[test]
    fn a_verdict_names_what_carries_it_and_a_doubt_is_not_one() {
        let store = Store::open_in_memory().expect("open");
        store.use_initiative("proj");
        let stale = episode(&store, "the-old-direction");
        let successor = episode(&store, "the-new-direction");
        crate::link(&store, &successor, &stale, EdgeType::Supersedes).expect("link");

        let doubted = episode(&store, "the-doubted-fact");
        let doubt = episode(&store, "why-i-doubt-it");
        crate::link(&store, &doubt, &doubted, EdgeType::Contradicts).expect("link");

        let verdicts = verdicts_against(&store).expect("read");
        assert_eq!(
            verdicts.get(&stale).map(|v| v.phrase()),
            Some("superseded by `the-new-direction`".to_string())
        );
        assert!(
            !verdicts.contains_key(&doubted),
            "a flag is an open question, not a verdict"
        );
    }

    /// A listing learns, per node, what `neighbours` would have told it: the
    /// verdict, the open doubt, and how many links there are to follow.
    #[test]
    fn standing_carries_the_verdict_the_doubt_and_the_link_count() {
        let store = Store::open_in_memory().expect("open");
        store.use_initiative("proj");
        let stale = episode(&store, "the-old-direction");
        let successor = episode(&store, "the-new-direction");
        crate::link(&store, &successor, &stale, EdgeType::Supersedes).expect("link");
        let source = episode(&store, "where-it-came-from");
        crate::link(&store, &stale, &source, EdgeType::DerivedFrom).expect("link");

        let doubted = episode(&store, "the-doubted-fact");
        let doubt = episode(&store, "why-i-doubt-it");
        crate::link(&store, &doubt, &doubted, EdgeType::Contradicts).expect("link");

        let lonely = episode(&store, "an-island");

        let got =
            standing(&store, &[stale.clone(), doubted.clone(), lonely.clone()]).expect("standing");
        assert_eq!(
            got[&stale].warning().as_deref(),
            Some("superseded by `the-new-direction`")
        );
        assert_eq!(got[&stale].links, 2, "the supersession and the source");
        assert_eq!(
            got[&doubted].warning().as_deref(),
            Some("disputed by `why-i-doubt-it`")
        );
        assert!(got[&doubted].verdict.is_none(), "a doubt is not a verdict");
        assert_eq!(got[&lonely], Default::default(), "nothing to say");

        // The node that DID the superseding is not itself under a verdict.
        let successor_standing = standing(&store, from_ref(&successor)).expect("standing");
        assert_eq!(successor_standing[&successor].warning(), None);
    }

    /// The verdict crosses initiatives — a replacement is often written where
    /// the work moved — but the link count is what `neighbours` shows here.
    #[test]
    fn a_verdict_crosses_scopes_and_the_link_count_does_not() {
        let store = Store::open_in_memory().expect("open");
        store.use_initiative("old-home");
        let stale = episode(&store, "the-old-plan");
        store.use_initiative("new-home");
        let successor = episode(&store, "the-new-plan");
        crate::link(&store, &successor, &stale, EdgeType::Supersedes).expect("link");

        store.use_initiative("old-home");
        let got = standing(&store, from_ref(&stale)).expect("standing");
        assert_eq!(
            got[&stale].warning().as_deref(),
            Some("superseded by `the-new-plan`")
        );
        assert_eq!(got[&stale].links, 0, "the successor lives in another scope");

        // Unscoped, every live peer counts — as unscoped `neighbours` lists it.
        store.clear_initiative();
        let got = standing(&store, from_ref(&stale)).expect("standing");
        assert_eq!(got[&stale].links, 1, "no scope, no filter");
        assert!(got[&stale].verdict.is_some());
    }
}
