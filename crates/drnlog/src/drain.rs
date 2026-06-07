//! Fixed-depth Drain-style clustering tree (He et al., 2017).
//!
//! The tree is indexed by `(token_count, leading-`depth-2`-slots)`. Each
//! leaf holds a list of candidate clusters; new tokenized lines are routed
//! to a leaf and either folded into the highest-similarity cluster there
//! (positions that disagree become [`Slot::Star`]) or spawn a new cluster.
//!
//! Drain operates on already-tokenized input — [`tokenizer::tokenize`] does
//! the work upstream. That keeps this module purely about clustering and
//! lets the same algorithm be reused for non-bitcoind logs (in principle)
//! by feeding it differently-recognized tokens.
//!
//! [`tokenizer::tokenize`]: crate::tokenizer::tokenize

use std::collections::HashMap;

use crate::tokenizer::{Token, TokenKind};

/// One position in a cluster template.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Slot {
    /// Literal text — matches only the same literal.
    Text(String),
    /// Typed wildcard — matches any token of this kind (e.g. any peer id).
    /// Introduced by the tokenizer.
    Typed(TokenKind),
    /// Universal wildcard — matches anything. Introduced by the Drain
    /// merge step when two clusters disagree at a position.
    Star,
}

impl Slot {
    /// Construct the owned counterpart of a `Token`.
    pub fn from_token(t: &Token<'_>) -> Self {
        match t.kind() {
            None => Slot::Text(t.text().to_string()),
            Some(k) => Slot::Typed(k),
        }
    }

    /// `true` if this slot accepts `tok` as compatible.
    fn matches(&self, tok: &Token<'_>) -> bool {
        match (self, tok) {
            (Slot::Star, _) => true,
            (Slot::Text(s), Token::Text(t)) => s == t,
            (Slot::Typed(k), other) => other.kind() == Some(*k),
            _ => false,
        }
    }
}

/// One cluster: a template + its occurrence count + metadata.
#[derive(Debug, Clone)]
pub struct Cluster {
    pub id: u64,
    pub template: Vec<Slot>,
    pub count: u64,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RouteKey {
    len: usize,
    prefix: Vec<Slot>,
}

/// A Drain-style clustering tree over pre-tokenized log lines.
#[derive(Debug, Clone)]
pub struct Drain {
    depth: usize,
    threshold: f64,
    tree: HashMap<RouteKey, Vec<Cluster>>,
    next_id: u64,
}

impl Drain {
    /// Build a fresh tree with the given hyperparameters.
    ///
    /// - `depth` ≥ 2. The first level routes by token count, the leaves
    ///   hold clusters, and `depth - 2` intermediate levels route on
    ///   leading-token slot kinds.
    /// - `threshold` ∈ [0, 1]. Higher → stricter (more clusters); lower →
    ///   looser (more `Star` slots).
    pub fn new(depth: usize, threshold: f64) -> Self {
        Self {
            depth: depth.max(2),
            threshold,
            tree: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn depth(&self) -> usize {
        self.depth
    }
    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Folds `tokens` into the tree, updating `count` / `last_seen` on
    /// match or spawning a new cluster. Borrowing, no allocation if the
    /// hot path lands on an existing cluster (apart from `String`s for
    /// `ts` and any newly-introduced `Star`s — `Star` is payload-free).
    pub fn add_tokens(&mut self, tokens: &[Token<'_>], ts: Option<&str>) {
        let key = self.route_key(tokens);
        let leaf = self.tree.entry(key).or_default();

        let mut best_idx: Option<usize> = None;
        let mut best_sim = 0.0;
        for (i, c) in leaf.iter().enumerate() {
            let sim = similarity(&c.template, tokens);
            if sim > best_sim {
                best_sim = sim;
                best_idx = Some(i);
            }
        }

        if let Some(idx) = best_idx
            && best_sim >= self.threshold
        {
            let c = &mut leaf[idx];
            for (slot, tok) in c.template.iter_mut().zip(tokens) {
                if !slot.matches(tok) {
                    *slot = Slot::Star;
                }
            }
            c.count += 1;
            if c.first_seen.is_none() {
                c.first_seen = ts.map(|s| s.to_string());
            }
            if let Some(t) = ts {
                c.last_seen = Some(t.to_string());
            }
            return;
        }

        let id = self.next_id;
        self.next_id += 1;
        let template: Vec<Slot> = tokens.iter().map(Slot::from_token).collect();
        leaf.push(Cluster {
            id,
            template,
            count: 1,
            first_seen: ts.map(|s| s.to_string()),
            last_seen: ts.map(|s| s.to_string()),
        });
    }

    /// Insert an already-built cluster (e.g. loaded from JSONL state).
    /// Updates `next_id` so future adds don't collide with loaded ids.
    pub fn insert_cluster(&mut self, cluster: Cluster) {
        if cluster.id >= self.next_id {
            self.next_id = cluster.id + 1;
        }
        let key = self.route_key_from_slots(&cluster.template);
        self.tree.entry(key).or_default().push(cluster);
    }

    /// Iterate over every cluster in the tree. No ordering guarantee.
    pub fn clusters(&self) -> impl Iterator<Item = &Cluster> {
        self.tree.values().flat_map(|v| v.iter())
    }

    fn route_key(&self, tokens: &[Token<'_>]) -> RouteKey {
        let n = self.depth.saturating_sub(2).min(tokens.len());
        let prefix = tokens[..n].iter().map(Slot::from_token).collect();
        RouteKey {
            len: tokens.len(),
            prefix,
        }
    }

    fn route_key_from_slots(&self, template: &[Slot]) -> RouteKey {
        let n = self.depth.saturating_sub(2).min(template.len());
        RouteKey {
            len: template.len(),
            prefix: template[..n].to_vec(),
        }
    }
}

fn similarity(template: &[Slot], tokens: &[Token<'_>]) -> f64 {
    if template.len() != tokens.len() {
        return 0.0;
    }
    if template.is_empty() {
        return 1.0;
    }
    let matches = template
        .iter()
        .zip(tokens)
        .filter(|(s, t)| s.matches(t))
        .count();
    matches as f64 / template.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenizer::tokenize;

    fn line(s: &str) -> Vec<Token<'_>> {
        tokenize(s)
    }

    #[test]
    fn identical_lines_fold_into_one_cluster() {
        let mut d = Drain::new(4, 0.5);
        for _ in 0..3 {
            d.add_tokens(&line("received: inv peer=3"), Some("2026-06-06T12:00:00Z"));
        }
        let clusters: Vec<&Cluster> = d.clusters().collect();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].count, 3);
    }

    #[test]
    fn typed_position_collapses_without_starring() {
        let mut d = Drain::new(4, 0.5);
        for n in 1..=5 {
            d.add_tokens(&line(&format!("received: inv peer={n}")), None);
        }
        let clusters: Vec<&Cluster> = d.clusters().collect();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].count, 5);
        // The peer=N position stayed as Typed(PeerId), not Star.
        let has_star = clusters[0].template.iter().any(|s| matches!(s, Slot::Star));
        let has_peer = clusters[0]
            .template
            .iter()
            .any(|s| matches!(s, Slot::Typed(TokenKind::PeerId)));
        assert!(!has_star, "no Star expected: {:?}", clusters[0].template);
        assert!(has_peer, "expected Typed(PeerId) in template");
    }

    #[test]
    fn differing_literal_position_demotes_to_star() {
        let mut d = Drain::new(4, 0.5);
        d.add_tokens(&line("The event named foo is done"), None);
        d.add_tokens(&line("The event named bar is done"), None);
        let clusters: Vec<&Cluster> = d.clusters().collect();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].count, 2);
        assert_eq!(
            clusters[0].template,
            vec![
                Slot::Text("The".into()),
                Slot::Text("event".into()),
                Slot::Text("named".into()),
                Slot::Star,
                Slot::Text("is".into()),
                Slot::Text("done".into()),
            ]
        );
    }

    #[test]
    fn disjoint_prefixes_split_into_separate_clusters() {
        let mut d = Drain::new(4, 0.5);
        d.add_tokens(&line("alpha one two"), None);
        d.add_tokens(&line("beta one two"), None);
        let clusters: Vec<&Cluster> = d.clusters().collect();
        assert_eq!(clusters.len(), 2);
        for c in &clusters {
            assert_eq!(c.count, 1);
        }
    }

    #[test]
    fn first_and_last_seen_track_timestamps() {
        let mut d = Drain::new(4, 0.5);
        d.add_tokens(&line("hello"), Some("2026-06-01T00:00:00Z"));
        d.add_tokens(&line("hello"), Some("2026-06-05T23:59:59Z"));
        let c = d.clusters().next().unwrap();
        assert_eq!(c.first_seen.as_deref(), Some("2026-06-01T00:00:00Z"));
        assert_eq!(c.last_seen.as_deref(), Some("2026-06-05T23:59:59Z"));
    }

    #[test]
    fn insert_cluster_round_trips() {
        let mut a = Drain::new(4, 0.5);
        a.add_tokens(&line("The event named foo is done"), None);
        a.add_tokens(&line("The event named bar is done"), None);

        let mut b = Drain::new(4, 0.5);
        for c in a.clusters() {
            b.insert_cluster(c.clone());
        }
        // Re-ingest a matching line via the reloaded tree.
        b.add_tokens(&line("The event named baz is done"), None);
        let clusters: Vec<&Cluster> = b.clusters().collect();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].count, 3);
    }
}
