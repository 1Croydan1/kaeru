//! Initiative discovery — `list_initiatives` returns the distinct set
//! of initiative names the substrate has seen at least one node attached
//! to. Mutations populate `node_initiative` automatically when the
//! `Store` has a `current_initiative` set.

use std::collections::BTreeMap;

use cozo::{DataValue, ScriptMutability};

use super::{NodeBrief, parse_brief};
use crate::errors::Result;
use crate::graph::NodeId;
use crate::store::Store;

/// Returns every initiative name that has at least one node attached
/// through the `node_initiative` junction. Sorted alphabetically.
///
/// Datalog rule-head deduplication produces distinct names; ordering is
/// applied at projection time so CLI output is stable.
pub fn list_initiatives(store: &Store) -> Result<Vec<String>> {
    let script = r#"
        ?[initiative] := *node_initiative{initiative, node_id}
        :order initiative
    "#;
    let rows = store.run_read(script)?;
    let names: Vec<String> = rows
        .rows
        .iter()
        .filter_map(|row| row.first().and_then(|v| v.get_str()).map(String::from))
        .collect();
    Ok(names)
}

/// The closest **existing** initiative to `requested`, or `None` when it
/// already exists exactly or nothing is close. A *suggestion* only — matching
/// and storage stay exact/case-sensitive; this is for a "did you mean …?" hint
/// on a miss, so it's deliberately forgiving (case-insensitive, substring, and
/// small edit-distance) where resolution is not.
pub fn suggest_initiative(store: &Store, requested: &str) -> Result<Option<String>> {
    let known = list_initiatives(store)?;
    let req = requested.trim();
    // Already a real initiative → nothing to suggest.
    if req.is_empty() || known.iter().any(|k| k == req) {
        return Ok(None);
    }
    let req_l = req.to_lowercase();

    // 1) case-insensitive exact — same name, different casing. The only rule
    //    that still short-circuits: nothing can beat it.
    if let Some(hit) = known.iter().find(|k| k.to_lowercase() == req_l) {
        return Ok(Some(hit.clone()));
    }

    // 2 & 3 are **scored together, not tried in order.** Containment used to
    // short-circuit, so `alpha_beta` suggested `alpha` — five characters
    // away — instead of `alpha-beta`, which is one. The one clear separator
    // typo in a 6,003-call corpus got the wrong answer from the very helper
    // meant to catch it (#86).
    //
    // A candidate is admissible if it is within the length-scaled edit
    // tolerance **or** one name contains the other (containment reaches
    // further than the tolerance — `vpn` inside `work-vpn` is five edits
    // away and still obviously related). Among the admissible, the closest by
    // edit distance wins, so the sharper signal decides.
    Ok(known
        .iter()
        .map(|k| (levenshtein(&req_l, &k.to_lowercase()), k))
        .filter(|(d, k)| {
            let kl = k.to_lowercase();
            *d <= (k.chars().count() / 3).max(2) || kl.contains(&req_l) || req_l.contains(&kl)
        })
        .min_by_key(|(d, k)| (*d, k.chars().count()))
        .map(|(_, k)| k.clone()))
}

/// Initiatives that look like different names for the same thing, as
/// `(a, b, why)` — the diagnosis for the disease `attach` is documented to
/// cure (#86).
///
/// Nothing surfaced this condition. `attach_node` is described verbatim as
/// "the repair primitive for initiative fragmentation" and was called seven
/// times in a 6,003-call corpus, while three separate projects sat split
/// across two or three names each. The cure was written and named; there was
/// no diagnosis.
///
/// Two signals, deliberately both:
///
/// - **normalised name** — case, and `-` / `_` / space / `.` folded to
///   nothing. This is what catches the pair created on the same day, from the
///   same session family, differing only in a separator.
/// - **shared members** — two initiatives holding some of the same nodes.
///   This is what catches a pair that shares no substring at all, which is
///   how a transliterated or translated alias looks; no string metric will
///   ever connect those.
///
/// Pairs only, never a verdict. Sub-scoping (`alpha` beside `alpha-api`) is a
/// legitimate pattern that looks identical to fragmentation from here, so
/// this reports and the reader decides.
pub fn near_duplicate_initiatives(store: &Store) -> Result<Vec<(String, String, String)>> {
    let known = list_initiatives(store)?;
    let mut out = Vec::new();

    for (i, a) in known.iter().enumerate() {
        for b in known.iter().skip(i + 1) {
            if normalise_initiative(a) == normalise_initiative(b) {
                out.push((
                    a.clone(),
                    b.clone(),
                    "same name apart from case and separators".to_string(),
                ));
                continue;
            }
            let shared = shared_member_count(store, a, b)?;
            if shared > 0 {
                out.push((a.clone(), b.clone(), format!("{shared} node(s) in both")));
            }
        }
    }
    Ok(out)
}

/// Case-folded, with every separator removed — so `alpha-beta`, `alpha_beta`
/// and `Alpha Beta` collapse onto one string.
fn normalise_initiative(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '-' | '_' | ' ' | '.'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// How many nodes are attached to both initiatives.
fn shared_member_count(store: &Store, a: &str, b: &str) -> Result<usize> {
    let mut params: BTreeMap<String, DataValue> = BTreeMap::new();
    params.insert("a".to_string(), DataValue::Str(a.into()));
    params.insert("b".to_string(), DataValue::Str(b.into()));
    let rows = store.db_ref().run_script(
        r#"
        ?[node_id] := *node_initiative{initiative: ia, node_id}, ia = $a,
                      *node_initiative{initiative: ib, node_id}, ib = $b
        "#,
        params,
        ScriptMutability::Immutable,
    )?;
    Ok(rows.rows.len())
}

/// Levenshtein edit distance (two-row DP). Small inputs (initiative names),
/// so the allocation is negligible.
fn levenshtein(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr = vec![0usize; b_chars.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_chars.len()]
}

/// Returns briefs for every node attached to `initiative` at NOW, with an
/// **explicit** initiative argument (not `Store::current_initiative`), so
/// it is safe to call concurrently from a multi-request server. Audit-event
/// nodes are excluded — they are operational noise, not shareable content.
pub fn nodes_in_initiative(store: &Store, initiative: &str) -> Result<Vec<NodeBrief>> {
    let excerpt_chars = store.config().body_excerpt_chars;
    let mut params: BTreeMap<String, DataValue> = BTreeMap::new();
    params.insert("init".to_string(), DataValue::Str(initiative.into()));

    let script = r#"
        ?[id, type, name, body, validity] := *node_initiative{initiative, node_id: id},
                                   initiative = $init,
                                   *node{id, type, name, body, validity @ 'NOW'},
                                   type != 'audit_event'
    "#;
    let rows = store
        .db_ref()
        .run_script(script, params, ScriptMutability::Immutable)?;

    let briefs = rows
        .rows
        .iter()
        .map(|row| parse_brief(row.as_slice(), excerpt_chars))
        .collect();
    Ok(briefs)
}

/// Counts non-audit nodes attached to `initiative` at NOW. A cheap `COUNT`
/// (no body loads, unlike `nodes_in_initiative`) — used by the capture nudge
/// to tell whether the initiative already holds anything worth linking a
/// fresh node to.
pub fn count_nodes_in_initiative(store: &Store, initiative: &str) -> Result<usize> {
    let mut params: BTreeMap<String, DataValue> = BTreeMap::new();
    params.insert("init".to_string(), DataValue::Str(initiative.into()));

    let script = r#"
        ?[count(id)] := *node_initiative{initiative, node_id: id},
                        initiative = $init,
                        *node{id, type @ 'NOW'},
                        type != 'audit_event'
    "#;
    let rows = store
        .db_ref()
        .run_script(script, params, ScriptMutability::Immutable)?;

    let count = rows
        .rows
        .first()
        .and_then(|row| row.first())
        .and_then(|v| v.get_int())
        .unwrap_or(0);
    Ok(count as usize)
}

/// Returns every `local` edge whose **both** endpoints are attached to
/// `initiative` at NOW, as `(src, dst, edge_type)`. Explicit initiative
/// argument (not `Store::current_initiative`) for concurrency safety. The
/// cloud serves this so a puller can rebuild the graph structure among
/// the nodes it materialises. Mirrors `export`'s both-endpoints scoping.
pub fn edges_in_initiative(
    store: &Store,
    initiative: &str,
) -> Result<Vec<(NodeId, NodeId, String, f64)>> {
    let mut params: BTreeMap<String, DataValue> = BTreeMap::new();
    params.insert("init".to_string(), DataValue::Str(initiative.into()));

    let script = r#"
        ?[src, dst, edge_type, weight] :=
            *edge{src, dst, edge_type, weight, dst_store @ 'NOW'},
            dst_store = 'local',
            *node_initiative{initiative, node_id: src},
            initiative = $init,
            *node_initiative{initiative: i2, node_id: dst},
            i2 = $init
    "#;
    let rows = store
        .db_ref()
        .run_script(script, params, ScriptMutability::Immutable)?;

    let edges = rows
        .rows
        .iter()
        .filter_map(|row| {
            let src = row.first().and_then(|v| v.get_str())?.to_string();
            let dst = row.get(1).and_then(|v| v.get_str())?.to_string();
            let edge_type = row.get(2).and_then(|v| v.get_str())?.to_string();
            let weight = row.get(3).and_then(|v| v.get_float()).unwrap_or(1.0);
            Some((src, dst, edge_type, weight))
        })
        .collect();
    Ok(edges)
}

#[cfg(test)]
mod tests {
    use super::{list_initiatives, near_duplicate_initiatives, suggest_initiative};
    use crate::jot;
    use crate::store::Store;

    fn seed(store: &Store, init: &str) {
        store.use_initiative(init);
        jot(store, "seed").unwrap();
    }

    #[test]
    fn use_initiative_trims_on_entry() {
        let store = Store::open_in_memory().expect("open");
        seed(&store, "auth-rewrite "); // trailing space
        assert!(
            list_initiatives(&store)
                .unwrap()
                .iter()
                .any(|n| n == "auth-rewrite"),
            "stored under the trimmed name"
        );
    }

    #[test]
    fn suggest_offers_the_closest_known_initiative() {
        let store = Store::open_in_memory().expect("open");
        seed(&store, "auth-rewrite");
        store.clear_initiative();

        // Exact match → nothing to suggest.
        assert_eq!(suggest_initiative(&store, "auth-rewrite").unwrap(), None);
        // Different casing.
        assert_eq!(
            suggest_initiative(&store, "Auth-Rewrite").unwrap(),
            Some("auth-rewrite".to_string())
        );
        // Truncation / substring.
        assert_eq!(
            suggest_initiative(&store, "auth").unwrap(),
            Some("auth-rewrite".to_string())
        );
        // A one-character typo.
        assert_eq!(
            suggest_initiative(&store, "auth-rewrit").unwrap(),
            Some("auth-rewrite".to_string())
        );
        // Nothing close → no suggestion.
        assert_eq!(
            suggest_initiative(&store, "totally-unrelated-xyz").unwrap(),
            None
        );
    }

    /// The one clear separator typo in a 6,003-call corpus got the wrong
    /// answer from the very helper meant to catch it: containment
    /// short-circuited, so `alpha_beta` suggested `alpha` — five characters
    /// away — over `alpha-beta`, which is one (#86).
    #[test]
    fn a_separator_typo_suggests_the_near_name_not_the_containing_one() {
        let store = Store::open_in_memory().expect("open");
        seed(&store, "alpha");
        seed(&store, "alpha-beta");

        assert_eq!(
            suggest_initiative(&store, "alpha_beta").unwrap().as_deref(),
            Some("alpha-beta"),
            "the closer name wins, whichever rule found it"
        );
    }

    /// Containment still reaches further than the edit tolerance, which is why
    /// it stays in: `vpn` is five edits from `work-vpn` and obviously related.
    #[test]
    fn containment_still_matches_beyond_the_edit_tolerance() {
        let store = Store::open_in_memory().expect("open");
        seed(&store, "work-vpn");

        assert_eq!(
            suggest_initiative(&store, "vpn").unwrap().as_deref(),
            Some("work-vpn")
        );
    }

    /// A pair differing only in a separator was created on the same day, from
    /// the same session family, and nothing ever said so. `attach` is
    /// documented as the repair primitive for this and was called seven times
    /// in the whole corpus — the cure was written, the diagnosis was not.
    #[test]
    fn names_differing_only_by_separator_are_reported_as_one_thing() {
        let store = Store::open_in_memory().expect("open");
        seed(&store, "alpha-beta");
        seed(&store, "alpha_beta");
        seed(&store, "something-else");

        let dupes = near_duplicate_initiatives(&store).unwrap();
        assert_eq!(dupes.len(), 1, "one pair, not three: {dupes:?}");
        let (a, b, why) = &dupes[0];
        assert!(
            (a == "alpha-beta" && b == "alpha_beta") || (a == "alpha_beta" && b == "alpha-beta"),
            "{dupes:?}"
        );
        assert!(why.contains("separator"), "and says why: {why}");
    }

    /// The expensive case in the report was a course under its English name
    /// and two transliterated aliases — no shared substring, far apart by edit
    /// distance. No string metric will ever connect those, so the second
    /// signal is membership: nodes attached to both names.
    #[test]
    fn a_translated_alias_is_caught_by_shared_members_not_by_spelling() {
        use crate::attach_node;

        let store = Store::open_in_memory().expect("open");
        store.use_initiative("n8n-agents");
        let id = jot(&store, "a lesson note").unwrap();
        seed(&store, "kurs-agentov");
        attach_node(&store, &id, "kurs-agentov").unwrap();

        let dupes = near_duplicate_initiatives(&store).unwrap();
        assert!(
            dupes
                .iter()
                .any(|(_, _, why)| why.contains("node(s) in both")),
            "names that share nothing but hold the same node: {dupes:?}"
        );
    }

    /// Unrelated initiatives are not a finding. A report that fires on
    /// everything is the same as no report.
    #[test]
    fn unrelated_initiatives_are_not_reported() {
        let store = Store::open_in_memory().expect("open");
        seed(&store, "auth-rewrite");
        seed(&store, "holiday-planning");

        assert!(near_duplicate_initiatives(&store).unwrap().is_empty());
    }
}
