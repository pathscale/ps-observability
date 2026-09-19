//! Condensing a settled page into levels a caller can widen from.
//!
//! A settled page is the object this crate has and a byte stream is not: roles,
//! names, bounds and visibility. Handing all of it to a model is the flat
//! context this exists to avoid — `crates.io` answers `semaphore` with 654
//! matches and a semantic tree is larger still.
//!
//! Five capabilities, ordered by what they cost and by how much they throw
//! away. A caller starts narrow and widens only when the narrow answer was not
//! enough:
//!
//! | capability | answers | shape |
//! |---|---|---|
//! | [`keywords`] | what is this page about | a ranked term list |
//! | [`summary`] | what does it say | short sentences, extractive |
//! | [`preview`] | what would I see | first N semantic nodes |
//! | [`structure`] | how is it laid out | the tree, names and no bodies |
//! | [`content`] | the whole thing | full text, the escape hatch |
//!
//! Two rules make the levels usable rather than lossy.
//!
//! **Every level says what it dropped.** A summary that does not say it is a
//! summary is indistinguishable from a short page, and a caller cannot tell
//! whether widening would help. Every return carries a [`Dropped`].
//!
//! **Extractive before abstractive.** [`summary`] is built from sentences the
//! page actually contains, so it can be checked against the page. A generated
//! summary cannot, and a wrong one is worse than none because it reads as
//! authority. Nothing here calls a model; a term-ranking pass over a settled
//! tree is microseconds, a model is milliseconds at best and a network hop at
//! worst, so an algorithm is tried first and measured.
//!
//! The keyword ranker is only as good as its null. [`keyword_margin`] is the
//! acceptance measure: terms from this page against terms from an unrelated
//! page of the same kind, and the margin is the result rather than the raw
//! score. An uncalibrated ranker measures common English, which is what
//! `karen-refuter` found under Rust tests — a 76.1% null floor beneath a 70%
//! threshold.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::SemanticNode;

/// What a level kept, against what was present.
///
/// `bytes_present` is the visible text the page carried — every name and value
/// a caller could have read. `bytes_kept` is what this level actually returned.
/// A level that returns everything reports `1.0 / 1.0`; that is [`content`],
/// and it is the escape hatch rather than the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Dropped {
    pub nodes_kept: usize,
    pub nodes_present: usize,
    pub bytes_kept: usize,
    pub bytes_present: usize,
}

impl Dropped {
    /// Fraction of nodes the level kept, `1.0` when the page was empty.
    #[must_use]
    pub fn node_ratio(&self) -> f64 {
        ratio(self.nodes_kept, self.nodes_present)
    }

    /// Fraction of text bytes the level kept, `1.0` when the page was empty.
    #[must_use]
    pub fn byte_ratio(&self) -> f64 {
        ratio(self.bytes_kept, self.bytes_present)
    }
}

fn ratio(kept: usize, present: usize) -> f64 {
    if present == 0 {
        1.0
    } else {
        kept as f64 / present as f64
    }
}

/// One level's answer, with the account of what it dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Condensed<T> {
    pub value: T,
    pub dropped: Dropped,
}

/// One ranked term.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RankedTerm {
    pub term: String,
    pub score: u32,
}

/// One node as a preview shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreviewNode {
    pub id: u64,
    pub role: String,
    pub name: String,
}

/// One node in the structural skeleton.
///
/// The name is the accessible name, which is what a caller navigates by. The
/// value and the attribute map are deliberately absent: a structure that
/// carried text bodies would not be a structure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StructureNode {
    pub id: u64,
    pub role: String,
    pub name: String,
    pub children: Vec<StructureNode>,
}

/// Every level over one settled page, so a cache stores one object.
///
/// Not a wire shape. The wire carries one level at a time ([`Condensed`]), and
/// this exists so a cache can answer any level from one settled revision
/// without condensing four times.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CondensedPage {
    pub keywords: Condensed<Vec<RankedTerm>>,
    pub summary: Condensed<Vec<String>>,
    pub preview: Condensed<Vec<PreviewNode>>,
    pub structure: Condensed<Vec<StructureNode>>,
    pub content: Condensed<String>,
}

/// One level, as it crosses the wire.
///
/// Level-shaped on purpose: a caller that asked for keywords must not be handed
/// the page's full text as a variant it did not request, which is the flat
/// context this whole module exists to avoid. A caller that wants more widens
/// with a second cheap request against the same cache entry.
///
/// Named `Condensation` rather than `Condensed` because `Condensed<T>` is the
/// per-level wrapper and one name cannot be both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "level", content = "value", rename_all = "camelCase")]
#[serde(rename_all_fields = "camelCase")]
pub enum Condensation {
    Keywords(Condensed<Vec<RankedTerm>>),
    Summary(Condensed<Vec<String>>),
    Preview(Condensed<Vec<PreviewNode>>),
    Structure(Condensed<Vec<StructureNode>>),
    Content(Condensed<String>),
}

impl Condensation {
    /// The level's name, for a response summary that has to say what came back
    /// without holding the value.
    #[must_use]
    pub fn level_name(&self) -> &'static str {
        match self {
            Condensation::Keywords(_) => "keywords",
            Condensation::Summary(_) => "summary",
            Condensation::Preview(_) => "preview",
            Condensation::Structure(_) => "structure",
            Condensation::Content(_) => "content",
        }
    }
}

/// The knobs a caller sets once. The defaults are the house's: the first
/// screenful, the first dozen terms, four sentences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CondenseOptions {
    pub preview_nodes: usize,
    pub summary_sentences: usize,
    pub max_keywords: usize,
}

impl Default for CondenseOptions {
    fn default() -> Self {
        Self {
            preview_nodes: 40,
            summary_sentences: 4,
            max_keywords: 24,
        }
    }
}

/// Which level a `Condense` request wants. Absent means [`Level::Keywords`],
/// the cheapest, so a caller that does not know yet pays the least.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Level {
    /// What is this page about.
    #[default]
    Keywords,
    /// What does it say.
    Summary,
    /// What would I see.
    Preview,
    /// How is it laid out.
    Structure,
    /// The whole thing, the escape hatch.
    Content,
}

impl Level {
    /// The level's own slice of a page, which is what crosses the wire.
    ///
    /// A cache holds the whole [`CondensedPage`] and answers every level from
    /// it, so widening is a second cheap request rather than a second settle.
    #[must_use]
    pub fn select(self, page: CondensedPage) -> Condensation {
        match self {
            Level::Keywords => Condensation::Keywords(page.keywords),
            Level::Summary => Condensation::Summary(page.summary),
            Level::Preview => Condensation::Preview(page.preview),
            Level::Structure => Condensation::Structure(page.structure),
            Level::Content => Condensation::Content(page.content),
        }
    }
}

/// Terms that carry no subject matter. A ranker without this measures English.
const STOPLIST: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "any", "can", "her", "was", "one",
    "our", "out", "day", "get", "has", "him", "his", "how", "its", "may", "new", "now", "old",
    "see", "two", "way", "who", "boy", "did", "use", "that", "with", "this", "from", "they",
    "will", "would", "there", "their", "what", "about", "which", "when", "make", "like", "time",
    "just", "know", "take", "into", "your", "some", "them", "than", "then", "only", "come", "over",
    "also", "back", "after", "could", "been", "have", "were", "more", "here", "does", "each",
    "many", "most", "been", "such", "even", "much", "these", "those", "being", "where", "while",
    "should", "because", "before", "between", "through", "during", "without",
];

/// How much a role's words are worth to the page's subject.
///
/// A heading is the page saying what it is about; a link or a button is a
/// deliberate label. Chrome and generic containers are discounted, not
/// dropped, because on a thin page they may be all there is.
fn role_weight(role: &str) -> u32 {
    match role {
        "h1" | "h2" | "h3" | "heading" | "title" => 5,
        "h4" | "h5" | "h6" => 4,
        "link" | "button" | "tab" | "menuitem" => 3,
        "label" | "navigation" | "banner" | "main" | "article" | "table" | "listitem" => 2,
        _ => 1,
    }
}

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() >= 3)
        .map(str::to_lowercase)
        .filter(|word| !STOPLIST.contains(&word.as_str()))
}

fn visible_text(node: &SemanticNode) -> String {
    match &node.value {
        Some(value) if !value.is_empty() => format!("{} {}", node.name, value),
        _ => node.name.clone(),
    }
}

fn text_bytes(nodes: &[SemanticNode]) -> usize {
    nodes
        .iter()
        .filter(|node| node.visible)
        .map(|node| visible_text(node).len())
        .sum()
}

fn ranked_terms(nodes: &[SemanticNode]) -> BTreeMap<String, u32> {
    let mut scores: BTreeMap<String, u32> = BTreeMap::new();
    for node in nodes.iter().filter(|node| node.visible) {
        let weight = role_weight(&node.role);
        for word in words(&visible_text(node)) {
            *scores.entry(word).or_insert(0) += weight;
        }
    }
    scores
}

/// What the page is about: terms ranked by frequency weighted by semantic role.
///
/// The score is not the answer. Use [`keyword_margin`] against an unrelated
/// page of the same kind before believing a term list; the margin is the
/// result.
#[must_use]
pub fn keywords(nodes: &[SemanticNode]) -> Condensed<Vec<RankedTerm>> {
    let scores = ranked_terms(nodes);
    let present = text_bytes(nodes);
    let contributing = nodes
        .iter()
        .filter(|node| node.visible && words(&visible_text(node)).next().is_some())
        .count();
    let mut terms: Vec<RankedTerm> = scores
        .into_iter()
        .map(|(term, score)| RankedTerm { term, score })
        .collect();
    terms.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.term.cmp(&b.term)));
    let kept = terms.iter().map(|term| term.term.len()).sum();
    Condensed {
        value: terms,
        dropped: Dropped {
            nodes_kept: contributing,
            nodes_present: nodes.len(),
            bytes_kept: kept,
            bytes_present: present,
        },
    }
}

/// The distinctiveness of a page's terms against a null page of the same kind.
///
/// Fraction of the target's terms that do not appear in the null at all. A
/// ranker whose margin is near zero is measuring common English, whatever its
/// raw score says. This is the acceptance test the house rule demands: null
/// floor first, then the number.
#[must_use]
pub fn keyword_margin(target: &[RankedTerm], null: &[RankedTerm]) -> f64 {
    if target.is_empty() {
        return 0.0;
    }
    let null: BTreeSet<&str> = null.iter().map(|term| term.term.as_str()).collect();
    let distinctive = target
        .iter()
        .filter(|term| !null.contains(term.term.as_str()))
        .count();
    distinctive as f64 / target.len() as f64
}

/// What the page says, as sentences the page actually contains.
///
/// Each visible node's text is one extractive unit. Units are scored by the
/// same weighted terms [`keywords`] ranks on, the best `max_sentences` are kept
/// and returned in document order. Nothing is generated, so every returned
/// sentence can be checked against the page.
#[must_use]
pub fn summary(nodes: &[SemanticNode], max_sentences: usize) -> Condensed<Vec<String>> {
    let scores = ranked_terms(nodes);
    let units: Vec<(usize, &SemanticNode)> = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.visible && !visible_text(node).trim().is_empty())
        .collect();
    let mut scored: Vec<(usize, u32)> = units
        .iter()
        .map(|(index, node)| {
            let score = words(&visible_text(node))
                .filter_map(|word| scores.get(&word).copied())
                .sum();
            (*index, score)
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut chosen: Vec<usize> = scored
        .into_iter()
        .take(max_sentences)
        .map(|(index, _)| index)
        .collect();
    chosen.sort_unstable();
    let sentences: Vec<String> = chosen
        .into_iter()
        .map(|index| visible_text(&nodes[index]).trim().to_string())
        .collect();
    let kept = sentences.iter().map(String::len).sum();
    let kept_nodes = sentences.len();
    Condensed {
        value: sentences,
        dropped: Dropped {
            nodes_kept: kept_nodes,
            nodes_present: nodes.len(),
            bytes_kept: kept,
            bytes_present: text_bytes(nodes),
        },
    }
}

/// What a caller would see first: the leading visible nodes, with their roles.
#[must_use]
pub fn preview(nodes: &[SemanticNode], limit: usize) -> Condensed<Vec<PreviewNode>> {
    let kept: Vec<PreviewNode> = nodes
        .iter()
        .filter(|node| node.visible)
        .take(limit)
        .map(|node| PreviewNode {
            id: node.id,
            role: node.role.clone(),
            name: node.name.clone(),
        })
        .collect();
    let bytes = kept
        .iter()
        .map(|node| node.role.len() + node.name.len())
        .sum();
    Condensed {
        value: kept,
        dropped: Dropped {
            nodes_kept: nodes.iter().filter(|node| node.visible).take(limit).count(),
            nodes_present: nodes.len(),
            bytes_kept: bytes,
            bytes_present: text_bytes(nodes),
        },
    }
}

/// How the page is laid out: the tree, names kept and text bodies dropped.
#[must_use]
pub fn structure(nodes: &[SemanticNode]) -> Condensed<Vec<StructureNode>> {
    let mut children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    let mut roots: Vec<u64> = Vec::new();
    let known: BTreeSet<u64> = nodes.iter().map(|node| node.id).collect();
    for node in nodes {
        match node.parent {
            Some(parent) if known.contains(&parent) => {
                children.entry(parent).or_default().push(node.id);
            }
            _ => roots.push(node.id),
        }
    }
    let by_id: BTreeMap<u64, &SemanticNode> = nodes.iter().map(|node| (node.id, node)).collect();
    fn build(
        id: u64,
        by_id: &BTreeMap<u64, &SemanticNode>,
        children: &BTreeMap<u64, Vec<u64>>,
    ) -> StructureNode {
        let node = by_id[&id];
        StructureNode {
            id,
            role: node.role.clone(),
            name: node.name.clone(),
            children: children
                .get(&id)
                .map(|ids| {
                    ids.iter()
                        .map(|child| build(*child, by_id, children))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
    let trees: Vec<StructureNode> = roots
        .into_iter()
        .map(|id| build(id, &by_id, &children))
        .collect();
    let kept = trees.iter().map(count_name).sum();
    Condensed {
        value: trees,
        dropped: Dropped {
            nodes_kept: nodes.len(),
            nodes_present: nodes.len(),
            bytes_kept: kept,
            bytes_present: text_bytes(nodes),
        },
    }
}

fn count_name(node: &StructureNode) -> usize {
    node.name.len() + node.children.iter().map(count_name).sum::<usize>()
}

/// The whole thing: the escape hatch, and the level a caller widens to last.
#[must_use]
pub fn content(nodes: &[SemanticNode]) -> Condensed<String> {
    let text: Vec<String> = nodes
        .iter()
        .filter(|node| node.visible)
        .map(visible_text)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let value = text.join("\n");
    Condensed {
        value,
        dropped: Dropped {
            nodes_kept: nodes.iter().filter(|node| node.visible).count(),
            nodes_present: nodes.len(),
            bytes_kept: text.iter().map(String::len).sum(),
            bytes_present: text_bytes(nodes),
        },
    }
}

/// Every level at once, so a cache stores one object per settled page.
#[must_use]
pub fn condense(nodes: &[SemanticNode], options: CondenseOptions) -> CondensedPage {
    let mut keywords = keywords(nodes);
    if keywords.value.len() > options.max_keywords {
        keywords.value.truncate(options.max_keywords);
        keywords.dropped.bytes_kept = keywords.value.iter().map(|term| term.term.len()).sum();
    }
    CondensedPage {
        keywords,
        summary: summary(nodes, options.summary_sentences),
        preview: preview(nodes, options.preview_nodes),
        structure: structure(nodes),
        content: content(nodes),
    }
}

/// What a cache key must include to be about a page that existed.
///
/// Keyed on the URL **and** the settle revision, never the fetch. Two loads of
/// the same URL can settle differently, and a cache keyed on the fetch serves a
/// page that never existed.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct SettleKey {
    pub url: Option<String>,
    pub revision: u64,
}

/// Recent condensations, keyed on settle.
///
/// Small and bounded: the expensive part was loading and settling the page, and
/// the condensation is the cheap part that is nevertheless worth not repeating.
/// Eviction is oldest-first, which is the right policy for a browsing session
/// that moves forward.
#[derive(Debug, Clone)]
pub struct CondensationCache {
    capacity: usize,
    order: VecDeque<SettleKey>,
    entries: BTreeMap<SettleKey, Arc<CondensedPage>>,
}

impl CondensationCache {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            order: VecDeque::new(),
            entries: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn get(&self, key: &SettleKey) -> Option<Arc<CondensedPage>> {
        self.entries.get(key).map(Arc::clone)
    }

    /// Insert a page, returning whatever was evicted. Re-inserting a key
    /// refreshes its position rather than growing the cache.
    pub fn insert(
        &mut self,
        key: SettleKey,
        page: Arc<CondensedPage>,
    ) -> Option<(SettleKey, Arc<CondensedPage>)> {
        if let Some(position) = self.order.iter().position(|existing| existing == &key) {
            self.order.remove(position);
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, page);
        if self.entries.len() <= self.capacity {
            return None;
        }
        let evicted = self.order.pop_front()?;
        let page = self.entries.remove(&evicted)?;
        Some((evicted, page))
    }

    /// Condense and store in one step, returning the cached object.
    pub fn condense(
        &mut self,
        key: SettleKey,
        nodes: &[SemanticNode],
        options: CondenseOptions,
    ) -> Arc<CondensedPage> {
        let page = Arc::new(condense(nodes, options));
        let _ = self.insert(key, Arc::clone(&page));
        page
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u64, parent: Option<u64>, role: &str, name: &str) -> SemanticNode {
        SemanticNode {
            dom_id: None,
            id,
            parent,
            role: role.to_string(),
            name: name.to_string(),
            value: None,
            enabled: true,
            focusable: false,
            viewport_fixed: false,
            visible: true,
            selected: false,
            bounds: None,
            slot: None,
        }
    }

    fn page() -> Vec<SemanticNode> {
        vec![
            node(1, None, "h1", "Borrowing and ownership in Rust"),
            node(
                2,
                Some(1),
                "p",
                "Ownership moves a value and borrowing lends it",
            ),
            node(3, Some(1), "h2", "Lifetimes"),
            node(4, Some(3), "p", "Lifetimes bound how long a borrow lives"),
            node(5, Some(1), "link", "Rustonomicon"),
        ]
    }

    #[test]
    fn keywords_rank_the_subject_above_the_chrome() {
        let terms = keywords(&page());
        let ranked: Vec<&str> = terms.value.iter().map(|term| term.term.as_str()).collect();
        assert!(ranked.contains(&"borrowing"), "{ranked:?}");
        assert!(ranked.contains(&"ownership"), "{ranked:?}");
        assert!(!ranked.contains(&"and"), "stoplist ran");
        assert!(terms.dropped.byte_ratio() < 1.0, "terms are not the page");
    }

    #[test]
    fn a_summary_is_made_of_sentences_the_page_contains() {
        let page = page();
        let summary = summary(&page, 2);
        assert_eq!(summary.value.len(), 2);
        for sentence in &summary.value {
            assert!(
                page.iter().any(|node| node.name == *sentence),
                "{sentence:?} is not on the page"
            );
        }
    }

    #[test]
    fn a_preview_is_the_leading_nodes_and_says_how_many_it_dropped() {
        let page = page();
        let preview = preview(&page, 3);
        assert_eq!(preview.value.len(), 3);
        assert_eq!(preview.dropped.nodes_present, 5);
        assert_eq!(preview.dropped.nodes_kept, 3);
    }

    #[test]
    fn structure_keeps_the_tree_and_drops_the_bodies() {
        let mut page = page();
        page[1].value = Some(
            "Ownership moves a value and borrowing lends it, which is a body a \
             structure should not carry"
                .to_string(),
        );
        let structure = structure(&page);
        assert_eq!(structure.value.len(), 1, "one root");
        assert_eq!(structure.dropped.node_ratio(), 1.0);
        assert!(structure.dropped.byte_ratio() < 1.0, "bodies were dropped");
        let root = &structure.value[0];
        assert_eq!(root.role, "h1");
        assert_eq!(root.children.len(), 3);
    }

    #[test]
    fn content_is_the_escape_hatch_and_keeps_everything_visible() {
        let content = content(&page());
        assert!(content.value.contains("Borrowing"));
        assert!(content.value.contains("Rustonomicon"));
        assert_eq!(content.dropped.byte_ratio(), 1.0);
    }

    #[test]
    fn the_null_margin_refuses_a_ranker_that_is_measuring_english() {
        let target = keywords(&page());
        let unrelated = keywords(&[node(1, None, "h1", "Weather and traffic in Osaka")]);
        let margin = keyword_margin(&target.value, &unrelated.value);
        assert!(
            margin > 0.5,
            "the pages should be mostly disjoint: {margin}"
        );

        // The same page against itself has no margin, whatever it scored.
        assert_eq!(keyword_margin(&target.value, &target.value), 0.0);
    }

    #[test]
    fn the_cache_is_keyed_on_settle_and_evicts_the_oldest() {
        let mut cache = CondensationCache::new(1);
        let one = SettleKey {
            url: Some("https://example.test/".into()),
            revision: 1,
        };
        let two = SettleKey {
            url: Some("https://example.test/".into()),
            revision: 2,
        };
        cache.condense(one.clone(), &page(), CondenseOptions::default());
        assert!(cache.get(&one).is_some());
        let evicted = cache.condense(two.clone(), &page(), CondenseOptions::default());
        assert!(cache.get(&two).is_some());
        assert!(cache.get(&one).is_none(), "same URL, different settle");
        // The return value is the object that was dropped, so a caller can say so.
        let _ = evicted;
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn reinserting_a_key_refreshes_rather_than_grows() {
        let mut cache = CondensationCache::new(2);
        let key = SettleKey {
            url: None,
            revision: 7,
        };
        cache.condense(key.clone(), &page(), CondenseOptions::default());
        cache.condense(key.clone(), &page(), CondenseOptions::default());
        assert_eq!(cache.len(), 1);
    }
}
