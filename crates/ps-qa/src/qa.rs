//! What a check is, and what its verdict means.
//!
//! # Why this exists
//!
//! A DOM-only test environment answers questions about a tree the user never
//! sees. It has no compositor, no hit-testing and no layout, so it reports a
//! node as fine while the screen shows nothing. A green suite over jsdom says
//! the code is self-consistent, not that the interface works.
//!
//! Two failure classes make the point, and both have shipped past a green unit
//! suite in a real application:
//!
//! - **Artwork that lays out and does not draw.** The semantic tree reports a
//!   node whether or not a pixel was painted, and a node can have a correct box
//!   and a correct stroke colour while drawing nothing at all. A renderer that
//!   parses each inline `<svg>` from that element's markup alone will resolve a
//!   reference into a shared sprite to nothing. jsdom cannot see this: its
//!   `<svg>` is a well-behaved object that never goes near a rasteriser.
//! - **Controls that only exist while hovered.** A test that never moves a
//!   pointer cannot see them at all.
//!
//! # What a check is
//!
//! A [`Check`] is a precondition, an action, and an assertion about the state
//! after it, all expressed against the running application. Every part is
//! observed rather than assumed, and [`Paints`](Expect::Paints) fails if the
//! element is in the tree with no box - which is the thing the semantic tree
//! alone will not tell you.
//!
//! Checks are data, and they belong to the application under test rather than
//! to this crate. See [`checks`] for where they are read from.
//!
//! ```sh
//! ps-qa qa           # every check
//! ps-qa qa icons     # one group
//! ```

use blitz_control_protocol::SemanticNode;
use std::collections::HashMap;

use crate::target::selector_matches_node;

/// A control to hover, and how many times to enter it.
///
/// `Some("Trigger")` and `Some(("Trigger", 5))` both parse, so a check that
/// only needs one entry says so in the shortest way and nothing already written
/// has to change.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Hover {
    /// Enter once.
    Once(String),
    /// Enter this many times, leaving between each.
    Times(String, u8),
}

impl Hover {
    /// The control to enter.
    pub fn target(&self) -> &str {
        match self {
            Hover::Once(name) | Hover::Times(name, _) => name,
        }
    }

    /// How many entries, never fewer than one.
    pub fn times(&self) -> u8 {
        match self {
            Hover::Once(_) => 1,
            Hover::Times(_, times) => (*times).max(1),
        }
    }
}

/// What a single check asserts once its action has run.
///
/// The full vocabulary is kept whether or not a check currently uses every
/// variant. These are the choices available when writing one, documented with
/// the failure each is right for, and a variant deleted for being momentarily
/// unused is a distinction the next person has to rediscover. `Grows` went
/// unconstructed the moment one check was strengthened to `PaintsMore`; it is
/// still the correct assertion for anything that mounts without painting.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Expect {
    /// The named node exists, is visible, and has a non-zero box.
    ///
    /// Non-zero is the part that matters. A node with a box of `0x0` is in the
    /// tree and on no screen, which is how a broken control passes a test that
    /// only asked whether it existed.
    Paints,
    /// A painted named node is enabled after the action.
    ///
    /// Use this for validation flows where entering a valid value unlocks a
    /// Save or Send control without changing its geometry.
    Enabled,
    /// A painted named node refuses input after the action.
    ///
    /// This is the observable completion state for actions such as clearing a
    /// saved value: the control remains visible, but cannot be invoked again
    /// until there is something new to act on.
    Disabled,
    /// No node matching the name exists.
    ///
    /// The assertion for a control that must *not* be reachable, and for
    /// checking that a destructive action did not fire.
    Absent,
    /// The node may remain in the tree, but nothing matching it is on screen.
    ///
    /// The inverse of [`Paints`](Expect::Paints), and the right question for
    /// anything that closes. A dismissed dialog is not removed: measured after
    /// a dialog's Cancel, the dialog's own content is still in the tree at
    /// `0x0 HIDDEN`. Asking for absence there reports a working control as
    /// broken, while asking for a box distinguishes dismissed from trapped -
    /// full size while open, nothing once closed.
    Vanishes,
    /// The count of matching nodes changed in the given direction.
    Grows,
    /// More matching nodes are *on screen* than before.
    ///
    /// [`Grows`](Expect::Grows) counts tree membership, which is the wrong
    /// question for a control that reveals something already mounted. The
    /// rename editor is built for the life of its component and merely hidden,
    /// so the node count does not move when it opens - only its box does.
    ///
    /// Counting painted nodes is also what makes the check falsifiable. A
    /// `Paints` assertion on `textbox` stays green while the pencil is dead,
    /// because the composer and the search field are textboxes that always
    /// paint; verified by reintroducing the bug and watching it pass.
    PaintsMore,
    /// A repeated rendered family's members change while its viewport size may stay fixed.
    ///
    /// Virtualized pagination replaces the five painted rows with the next
    /// five rather than mounting ten. Count assertions therefore reject a
    /// working pager. This compares the matching semantic identities and names
    /// so replacing or recycling rows both prove that the page advanced.
    FamilyChanges,
    /// The count of matching nodes did not change.
    Holds,
    /// The count of matching nodes equals [`Check::expect_count`].
    Count,
    /// A node matching *both* a name and a role paints.
    ///
    /// The precise form of [`Paints`](Expect::Paints), for the common case
    /// where a control and the thing it opens share an accessible name.
    /// An in-place rename control does exactly that: `Rename <subject>`
    /// resolves to a button that always paints *and* a textbox that paints only
    /// while editing, so a name-only `Paints` is satisfied by the pencil
    /// whether or not the editor ever opens.
    ///
    /// A count-based assertion is not the answer either. `PaintsMore` over
    /// every `textbox` in the tree is fragile to whatever else happens to be
    /// on screen - a composer, a search field, an editor left open by an
    /// earlier check - and reported `2 -> 2` on a run where the editor
    /// demonstrably opened. Asking about one node by name and role is the
    /// question the check actually means.
    ///
    /// Written as `role:name`, e.g. `textbox:Rename thing`.
    ///
    /// Judged on geometry, not on the tree's `visible` flag, because the two
    /// disagree. Measured on one node at one instant: `paint` reported the
    /// rename editor `300.0x21.1 at 87,236 fg=#b0b5b9ff opacity=1.00 Visible`
    /// while `dom` reported the same id `HIDDEN`. `paint` reads what the
    /// render pass resolved; `visible` walks ancestors looking for
    /// `display:none` and `aria-hidden`, and a wrapper whose class no longer
    /// says `hidden` was still carrying it in the style tree while its subtree
    /// laid out and drew.
    ///
    /// So a check that trusts `visible` calls a control a person can see and
    /// type into dead. Geometry plus a position is the honest question here.
    PaintsNamed,
    /// Every painted node in the matching family occupies its own position.
    ///
    /// A repeated family can expose correct state and non-zero boxes while
    /// every member is stacked on the same coordinates. This is the rendered
    /// contract for layouts such as a colour flower.
    DistinctPositions,
    /// Every painted node in the matching family stays inside one comparison box.
    ///
    /// Distinct positions alone do not prove a composition is intact: every
    /// petal in a colour flower can occupy a different coordinate while the
    /// whole family is detached from its wheel. `compare` names the semantic
    /// container whose rendered bounds own the family.
    ContainedBy,
    /// The exact accessible name selected for this check's click still paints.
    ///
    /// Use this for a row action whose label includes the row identity. It
    /// proves that exact row survived without comparing a broad family count
    /// that hover-revealed neighbours can legitimately change.
    TargetPaints,
    /// The subject's box is the size it is supposed to be.
    ///
    /// Every other assertion here is satisfied by *any* non-zero box, which is
    /// how a control with a correct role, a correct name and correct text
    /// passes while rendering as a sliver. A pill that should stand 24px tall
    /// and comes out at 8 has lost its padding or its line-height; a menu that
    /// should be at least 190px wide and comes out at 40 has lost its
    /// min-width. Both are the styling artefacts that make a component look
    /// broken while the semantic tree reports it as fine.
    ///
    /// Written as `WxH` in `expect_size`, either side optional and each a
    /// minimum unless prefixed: `24` is at least 24, `=24` is exactly 24,
    /// `<=24` is at most. Tolerance is a pixel, because layout rounds.
    Measures,
    /// The named subject paints above the named comparison control.
    ///
    /// This is a rendered-order assertion, not DOM order. It verifies list
    /// placement using the boxes a person actually sees.
    Above,
    /// The named subject paints entirely to the right of the comparison box.
    ///
    /// This guards desktop compositions whose controls must sit beside a
    /// primary visual rather than falling into the narrow/mobile stack.
    RightOf,
    /// The subject and comparison boxes share the same vertical center.
    ///
    /// `RightOf` catches a desktop composition collapsing into a stack, but it
    /// still passes when the primary visual is pinned to the top of a much
    /// taller controls column. This assertion guards the authored balance of
    /// the two side-by-side regions.
    CenterAlignedY,
    /// The rendered pixels stay identical across the declared pointer abuse.
    ///
    /// This is deliberately a frame assertion rather than a DOM-count
    /// assertion. A duplicated fixed child still exists once in the DOM and
    /// has one layout box, but semi-transparent shadows darken on every
    /// incremental resolve. The live runner captures before and after
    /// [`after_prepare_hover`](Check::after_prepare_hover), with the pointer
    /// returned to the same non-hover state.
    PixelsHold,
    /// The first stable neutral frame equals the neutral frame after one hover.
    ///
    /// This catches controls whose initial display list is malformed until a
    /// pointer invalidation repairs it. The semantic tree and layout boxes can
    /// be correct in both frames, so a geometry-only assertion stays green.
    PixelsHoldAfterHover,
    /// Hovering the declared control visibly changes the captured region.
    ///
    /// This guards authored hover feedback through rendered pixels. Semantic
    /// inspection cannot see a `data-focused` style change, and merely proving
    /// the pointer event was dispatched does not prove the person received
    /// any feedback from it.
    PixelsChange,
    /// The captured region contains pixels visibly distinct from its backdrop.
    ///
    /// Geometry and resolved stroke properties both stay valid when an SVG
    /// references a missing sprite symbol. Raster ink is the observable that
    /// distinguishes a real icon from an empty box.
    VisibleInk,
    /// The captured control contains visible pixels away from its border.
    ///
    /// Use this for closed selectors and labeled fields: their border makes a
    /// whole-region ink check pass even when the value a person needs to read
    /// is blank. The center inset excludes that chrome and proves content.
    InteriorInk,
    /// The subject's resolved background colour is fully opaque.
    ///
    /// A translucent child over an animated gradient cannot have a flat fill,
    /// regardless of stacking contexts: the parent's pixels necessarily show
    /// through. The live runner reads the colour Blitz hands to paint and
    /// requires an alpha channel of `ff`.
    OpaqueBackground,
    /// The subject's resolved background colour has zero alpha.
    ///
    /// This is stricter than merely being translucent: an opacity slider at its
    /// floor must admit the native backdrop completely, not leave a pale film.
    TransparentBackground,
    /// The subject paints at opacity 1 rather than fading all of its content.
    ///
    /// Use this when a disabled control may mute its background but must keep
    /// an icon or label crisp. Applying opacity to the whole control also
    /// fades the content and is the regression this distinguishes.
    FullOpacity,
    /// The platform window is transparent and its installed glass tint has
    /// zero alpha.
    ///
    /// Renderer paint cannot answer this: AppKit composites the window and its
    /// glass view outside the document. This expectation reads the runtime's
    /// applied native composition and rejects a merely transparent CSS root
    /// sitting over an accent-coloured native sheet.
    TransparentWindowTint,
    /// Every named painted node matching the subject meets its contrast floor.
    ///
    /// Text and labeled controls use 4.5:1. Graphical form chrome and
    /// icon-only actions use 3:1, sampled from the paint property that actually
    /// identifies them (foreground, border, or fill).
    Contrast,
    /// The subject's resolved font size increases after the action.
    ///
    /// This verifies interface scaling against the computed style Blitz hands
    /// to layout. Enlarging one control box is not enough: fixed-size text can
    /// remain unreadably small inside it while a geometry-only check passes.
    FontSizeGrows,
    /// The exact semantic node's exposed value changed after the action.
    ///
    /// This is the outcome for sliders, switches, and other value-bearing
    /// controls whose geometry does not change when they work. The node id is
    /// carried from the before snapshot so a repeated accessible name cannot
    /// satisfy the assertion with a neighbouring row.
    ValueChanges,
    /// The exact semantic node's selected/pressed state changed.
    ///
    /// Radios and `aria-pressed` buttons expose selection as a boolean rather
    /// than inventing a string value. This follows the activated node id, so a
    /// neighbouring swatch cannot satisfy the verdict.
    SelectionChanges,
    /// The exact subject node's accessible name changed after the action.
    ///
    /// Use this for status text that reports a completed refresh or re-check.
    /// Following the node id prevents an unrelated new status row from
    /// satisfying the outcome.
    NameChanges,
}

/// One thing that must be true of the running panel.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// Stable name for this one check, so a failure can be re-run alone.
    ///
    /// The group is too coarse for that: chasing one fix meant re-running its
    /// neighbours every time, and each run drives the real app. `what` is prose
    /// and changes when the wording improves, so it cannot be the handle.
    /// This is the handle - `ps-qa qa rename-opens-editor`.
    pub id: String,
    /// Group, so a failing area can be re-run alone.
    pub group: String,
    /// What this proves, in the words you would use to report it.
    pub what: String,
    /// Press this first, to reach the surface the check is about.
    ///
    /// Checks run in sequence against one instance and start wherever the app
    /// opens, so anything not on that first surface is unreachable without a
    /// navigation step. A control that lives on only one surface fails with
    /// "no visible, enabled, sized button" when the check never got there,
    /// which reads as a missing control rather than a missing navigation.
    pub open: Option<String>,
    /// Activate this semantic control after navigation and before measuring
    /// the action baseline.
    ///
    /// This gives a check an explicit state precondition without making it
    /// depend on a preceding check. For example, select one tab here, then
    /// activate another and require its selected state to change.
    #[serde(default)]
    pub prepare: Option<String>,
    /// Skip [`prepare`](Self::prepare) when this rendered target already paints.
    ///
    /// Component sweeps intentionally reuse one native host per component for
    /// speed. An earlier outcome may therefore leave a menu open. Activating
    /// its trigger again would close the menu and turn shared-host execution
    /// into a false failure, while omitting preparation would make the same
    /// check impossible to run by id against a fresh host. This selector makes
    /// the precondition idempotent in both cases.
    #[serde(default)]
    pub prepare_unless: Option<String>,
    /// Use a real pointer press for [`prepare`](Self::prepare).
    ///
    /// Focus-sensitive popovers can legitimately distinguish a human press
    /// from a bare semantic activation. Keep this separate from `press`, since
    /// the prepared trigger and measured action may have different roles.
    #[serde(default)]
    pub prepare_press: bool,
    /// Send this key to [`prepare`](Self::prepare) instead of activating it.
    ///
    /// This keeps focus-sensitive controls self-contained: a select check can
    /// open its listbox with the component's authored keyboard contract, then
    /// measure an option selection without depending on an earlier check.
    #[serde(default)]
    pub prepare_key: Option<String>,
    /// Hover this node first, if the control is revealed on hover.
    ///
    /// Either a name, or a name and a count: `hover: Some("Trigger")` enters
    /// once, `hover: Some(("Trigger", 5))` enters five times, leaving between
    /// each.
    ///
    /// The count is how a check reaches the defects a single hover cannot see.
    /// A pill whose hover appends a shadow layer and never removes it looks
    /// correct the first time and accumulates from the second, so it is
    /// invisible both to a check that hovers once and to anyone who does not
    /// happen to hover twice by hand. Leaving between entries is the part that
    /// matters: hovering the same node twice with no departure is one hover as
    /// far as the renderer is concerned, and the defect lives in the
    /// enter/leave pair.
    ///
    /// When the count is greater than one, the harness compares the tree after
    /// the first completed entry with the tree after the last. Comparing equal
    /// hover states separates retained nodes from a legitimate hover affordance,
    /// and a single entry does not mistake unrelated asynchronous rendering for
    /// accumulation.
    pub hover: Option<Hover>,
    /// Skip [`hover`](Self::hover) when this rendered target already paints.
    ///
    /// A preceding check may intentionally leave the dialog or row action that
    /// hover would reveal open. Re-hovering the covered row is impossible and
    /// unnecessary; an isolated rerun still performs the hover when the target
    /// is absent.
    #[serde(default)]
    pub hover_unless: Option<String>,
    /// Repeatedly hover this node after [`prepare`](Self::prepare).
    ///
    /// Menus do not expose their items until their trigger has opened them, so
    /// the ordinary pre-prepare hover cannot exercise pointer-driven retained
    /// painting inside an overlay. This action exists for that ordering and
    /// leaves between entries exactly like [`hover`](Self::hover).
    #[serde(default)]
    pub after_prepare_hover: Option<Hover>,
    /// Scroll this semantic region into view before a rendered baseline.
    ///
    /// Deferred application sections commonly expose a header before their
    /// body. Revealing that header is an observation/precondition, not a fake
    /// click, and it lets the app mount the subject before its first hover.
    #[serde(default)]
    pub reveal_before_capture: Option<String>,
    /// Focus this field and enter [`setup_text`](Self::setup_text) after
    /// navigation, before preparation and the measured action.
    ///
    /// This makes multi-step checks independent of state left by earlier
    /// checks. A filtered list can establish its query here, then click a row
    /// action and measure the editor outcome with `type_into`/`key`.
    #[serde(default)]
    pub setup_type_into: Option<String>,
    /// Literal value entered into [`setup_type_into`](Self::setup_type_into).
    #[serde(default)]
    pub setup_text: Option<String>,
    /// Click this node, if the check is about an action.
    pub click: Option<String>,
    /// Focus this named text field and enter [`text`](Self::text).
    pub type_into: Option<String>,
    /// Literal text to enter after the click step.
    pub text: Option<String>,
    /// A final named key or chord such as Enter, Escape, or Meta+2, sent to
    /// `type_into`.
    pub key: Option<String>,
    /// Focus this named semantic control before sending `key`.
    ///
    /// Unlike `type_into`, this accepts any value-bearing role, including a
    /// slider. Name resolution produces a node id before the action is sent.
    pub key_on: Option<String>,
    /// Move over this semantic node and send real wheel input after other actions.
    #[serde(default)]
    pub scroll_over: Option<String>,
    /// Number of wheel events for [`scroll_over`](Self::scroll_over).
    #[serde(default)]
    pub scroll_ticks: usize,
    /// Vertical pixels per wheel event. Negative scrolls down.
    #[serde(default)]
    pub scroll_delta: f64,
    /// The second named node for a relative-position expectation.
    pub compare: Option<String>,
    /// Target size for [`Measures`](Expect::Measures), as `WxH`.
    ///
    /// Either side may be empty to leave that axis unasserted: `x24` asserts
    /// height alone, which is the common case for a control whose width is
    /// content-driven.
    #[serde(default)]
    pub expect_size: Option<String>,
    /// Exact family size for [`Expect::Count`].
    #[serde(default)]
    pub expect_count: Option<usize>,
    /// Additional controls covered by this same rendered contract.
    ///
    /// Use this only for a repeated family produced from one component and
    /// one data array, such as every `Offer {model}` checkbox. The action and
    /// verdict still address one exact semantic node; these selectors let the
    /// inventory credit its identical siblings without pretending a substring
    /// chosen for activation is itself an outcome.
    #[serde(default)]
    pub covers: Vec<String>,
    /// Opt into generic coordinate-pointer activation instead of semantic
    /// node activation. Application suites omit this unless they explicitly
    /// intend to test hit-testing.
    #[serde(default)]
    pub press: bool,
    /// Optional unmeasured quiet window after this verdict.
    ///
    /// Use this only when an application paints optimistically and then begins
    /// durable work that temporarily occupies its control transport. The
    /// rendered action still has to pass the one-second budget.
    #[serde(default)]
    pub settle_after_ms: u64,
    /// Deadline for this check's rendered outcome.
    ///
    /// Ordinary interactions keep the 900ms contract. A declared backend or
    /// network round trip may opt into a larger explicit budget without
    /// weakening every button in the sweep.
    #[serde(default)]
    pub outcome_timeout_ms: u64,
    /// Require the successful rendered outcome to remain present while the
    /// complete live semantic snapshot stays unchanged for this long.
    ///
    /// Navigation can expose a heading in its first partial frame and mount
    /// the rest of the page afterward. A name-only outcome then measures tab
    /// activation rather than a usable page. This window is part of outcome
    /// latency: every later tree change restarts it, so staged content cannot
    /// be reported as a fast completed render.
    #[serde(default)]
    pub stable_for_ms: u64,
    /// Run this check only after every ordinary shared-instance outcome.
    ///
    /// A destructive sequence may deliberately remove fixture state that
    /// controls on another surface still need. Surface affinity cannot order
    /// that safely: it groups by mount cost, so a project reset would otherwise
    /// precede later Settings and Home checks. The runner keeps destructive
    /// checks ordered, but moves their whole sequence to the final tail.
    #[serde(default)]
    pub destructive: bool,
    /// The node the assertion is about.
    pub subject: String,
    pub expect: Expect,
}

/// Every check, in the order they run, read from the application's own files.
///
/// # Why these are not compiled in
///
/// They used to be Rust, in a `tests/ps-qa/` module inside this crate, which was
/// wrong twice over. It put one product's promises - its control names, its
/// section labels, its fixture names - inside a general harness, so pointing
/// this binary at a second application meant editing the harness.
/// And it made changing a selector a recompile: correcting one hardcoded
/// fixture name after a profile rebuild touched 15 references across 6 files
/// and needed a build to try.
///
/// A check is data. It is a precondition, an action and an assertion, with no
/// behaviour of its own, so it belongs in a file the application owns and
/// anybody can edit between runs.
///
/// Read from `<dir>/*.ron`, where `<dir>` is `--checks <path>` or `tests/ps-qa/`
/// under the working directory. Files are read in name order and concatenated,
/// so the group order is the filename order.
pub fn default_checks_path() -> std::path::PathBuf {
    std::path::PathBuf::from("tests/ps-qa")
}

pub fn checks(dir: Option<&std::path::Path>) -> Result<Vec<Check>, String> {
    let dir = dir
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(default_checks_path);
    if !dir.is_dir() {
        return Err(format!(
            "no checks at {}. Point --checks at the application's check \
             directory.",
            dir.display()
        ));
    }
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map_err(|error| format!("could not read {}: {error}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "ron"))
        .collect();
    // Name order, so a run is reproducible rather than dependent on whatever
    // order the filesystem happens to hand back.
    files.sort();

    let mut all = Vec::new();
    let mut ids = HashMap::new();
    for file in files {
        let text = std::fs::read_to_string(&file)
            .map_err(|error| format!("could not read {}: {error}", file.display()))?;
        let group: Vec<Check> = ron::from_str(&text)
            .map_err(|error| format!("could not parse {}: {error}", file.display()))?;
        for check in &group {
            validate_check(check, &file, &mut ids)?;
        }
        all.extend(group);
    }
    Ok(all)
}

fn validate_check(
    check: &Check,
    file: &std::path::Path,
    ids: &mut HashMap<String, std::path::PathBuf>,
) -> Result<(), String> {
    if let Some(previous) = ids.insert(check.id.clone(), file.to_path_buf()) {
        return Err(format!(
            "duplicate check id {:?} in {} (already declared in {})",
            check.id,
            file.display(),
            previous.display()
        ));
    }

    if check.expect == Expect::Vanishes
        && check.prepare_key.is_some()
        && check.prepare_unless.as_deref() == Some(check.subject.as_str())
        && check.click.is_none()
        && check.key.is_none()
        && check.type_into.is_none()
    {
        return Err(format!(
            concat!(
                "{}: check {:?} can pass without opening {:?}: prepare_key replaces prepare ",
                "activation and is skipped when prepare_unless already paints; open with ",
                "prepare, then send the dismiss key with key and key_on"
            ),
            file.display(),
            check.id,
            check.subject
        ));
    }

    if check.setup_type_into.is_some() != check.setup_text.is_some() {
        return Err(format!(
            "{}: check {:?} must declare setup_type_into and setup_text together",
            file.display(),
            check.id
        ));
    }

    if check.scroll_over.is_some() && (check.scroll_ticks == 0 || check.scroll_delta == 0.0) {
        return Err(format!(
            "{}: check {:?} must declare non-zero scroll_ticks and scroll_delta with scroll_over",
            file.display(),
            check.id,
        ));
    }

    if check.expect == Expect::Count && check.expect_count.is_none() {
        return Err(format!(
            "{}: check {:?} must declare expect_count with Count",
            file.display(),
            check.id,
        ));
    }

    Ok(())
}

fn matching<'a>(nodes: &'a [SemanticNode], want: &str) -> Vec<&'a SemanticNode> {
    nodes
        .iter()
        .filter(|node| selector_matches_node(node, want))
        .collect()
}

/// Whether a node is on screen with a box worth painting.
///
/// A zero-area box is the failure this exists to catch: present in the tree,
/// absent from the window.
fn paints(node: &SemanticNode) -> bool {
    /*
     * Geometry alone, because `visible` and the renderer disagree.
     *
     * Measured on one node at one instant: `paint` reported the rename editor
     * `300.0x21.1 at 87,236 opacity=1.00 Visible` while the semantic tree
     * reported that same id `HIDDEN`. `visible` walks ancestors for
     * `display:none` and `aria-hidden`, and a wrapper whose class no longer
     * says `hidden` was still carrying it in the style tree while its subtree
     * laid out and drew.
     *
     * Trusting the flag called controls dead that a person can see and use -
     * the icons group reported "246 exist, none paints" for an app visibly
     * full of icons. A non-zero box at a real position is what can be checked
     * honestly from here; `ps-qa paint` is the tool for the pixels themselves.
     */
    node.bounds.is_some_and(|b| b[2] > 0.0 && b[3] > 0.0)
}

/// The verdict for one check, given the tree before and after its action.
pub fn verdict(
    check: &Check,
    before: &[SemanticNode],
    after: &[SemanticNode],
) -> Result<(), String> {
    let found = matching(after, &check.subject);
    match check.expect {
        Expect::Vanishes => {
            /*
             * Both signals, because neither answers this alone.
             *
             * `paints` is geometry only, and deliberately so: `visible` walks
             * ancestors for `display:none` and `aria-hidden` and reported a
             * screen full of icons as painting nothing. But a closed overlay
             * keeps its box. Measured on Select: Escape closes the listbox
             * correctly and both options stay in the tree at 1170x36, now
             * hidden, so geometry alone called a working component broken.
             *
             * `Paints` still asks about geometry, where trusting `visible`
             * would reintroduce the false negative. Only this arm, which asks
             * whether something went away, needs to hear that it did.
             */
            let on_screen: Vec<&SemanticNode> = found
                .iter()
                .copied()
                .filter(|node| node.visible && paints(node))
                .collect();
            if let Some(node) = on_screen.first() {
                let b = node.bounds.unwrap_or([0.0; 4]);
                return Err(format!(
                    "{:?} is still on screen at {:.0}x{:.0}; it did not close",
                    check.subject, b[2], b[3]
                ));
            }
        }
        Expect::Paints => {
            if found.is_empty() {
                return Err(format!("no node matching {:?} exists", check.subject));
            }
            let broken: Vec<_> = found.iter().copied().filter(|node| !paints(node)).collect();
            if !broken.is_empty() {
                /*
                 * Say which half of "paints" failed.
                 *
                 * Hidden-but-sized and visible-but-zero-area are different
                 * bugs: the first is a node the panel deliberately keeps
                 * offscreen, the second is a control the user is meant to see
                 * and cannot. Reporting them as one message sent me looking at
                 * the wrong one.
                 */
                let hidden = broken.iter().filter(|node| !node.visible).count();
                let zero = broken
                    .iter()
                    .filter(|node| {
                        node.visible && !node.bounds.is_some_and(|b| b[2] > 0.0 && b[3] > 0.0)
                    })
                    .count();
                let boxes: Vec<String> = broken
                    .iter()
                    .take(3)
                    .map(|node| {
                        let size = node
                            .bounds
                            .map(|b| format!("{:.0}x{:.0}", b[2], b[3]))
                            .unwrap_or_else(|| "no box".into());
                        format!("{size}{}", if node.visible { "" } else { " hidden" })
                    })
                    .collect();
                return Err(format!(
                    "{} of {} node(s) matching {:?} do not paint: \
                     {hidden} hidden, {zero} visible with no area ({})",
                    broken.len(),
                    found.len(),
                    check.subject,
                    boxes.join(", ")
                ));
            }
        }
        Expect::Enabled => {
            if found.iter().any(|node| paints(node) && node.enabled) {
                return Ok(());
            }
            let states = found
                .iter()
                .take(3)
                .map(|node| {
                    format!(
                        "id={} role={:?} name={:?} enabled={} bounds={:?}",
                        node.id, node.role, node.name, node.enabled, node.bounds
                    )
                })
                .collect::<Vec<_>>();
            return Err(format!(
                "no painted, enabled node matching {:?} ({})",
                check.subject,
                states.join(", ")
            ));
        }
        Expect::Disabled => {
            if found.iter().any(|node| paints(node) && !node.enabled) {
                return Ok(());
            }
            let states = found
                .iter()
                .take(3)
                .map(|node| {
                    format!(
                        "id={} role={:?} name={:?} enabled={} bounds={:?}",
                        node.id, node.role, node.name, node.enabled, node.bounds
                    )
                })
                .collect::<Vec<_>>();
            return Err(format!(
                "no painted, disabled node matching {:?} ({})",
                check.subject,
                states.join(", ")
            ));
        }
        Expect::Absent => {
            if !found.is_empty() {
                return Err(format!(
                    "{} node(s) matching {:?} should not exist",
                    found.len(),
                    check.subject
                ));
            }
        }
        Expect::PaintsNamed => {
            if !found.iter().any(|node| paints(node)) {
                let state = found
                    .iter()
                    .map(|node| {
                        format!(
                            "id={} parent={:?} visible={} bounds={:?}",
                            node.id, node.parent, node.visible, node.bounds
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                let other_painted = check
                    .subject
                    .split_once(':')
                    .map(|(role, name)| {
                        after
                            .iter()
                            .filter(|node| {
                                !node.role.eq_ignore_ascii_case(role)
                                    && crate::target::name_matches(&node.name, name)
                                    && paints(node)
                            })
                            .take(3)
                            .map(|node| format!("{} id={}", node.role, node.id))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let other_painted = if other_painted.is_empty() {
                    "none".to_owned()
                } else {
                    other_painted
                };
                return Err(format!(
                    "no painted node matching {:?} has a box ({} in the tree: {state}); \
                     other painted matches: {other_painted}",
                    check.subject,
                    found.len(),
                ));
            }
        }
        Expect::DistinctPositions => {
            let painted: Vec<_> = found.iter().copied().filter(|node| paints(node)).collect();
            if painted.len() < 2 {
                return Err(format!(
                    "{:?} has {} painted match(es); expected a positioned family",
                    check.subject,
                    painted.len()
                ));
            }
            let positions: std::collections::HashSet<(i64, i64)> = painted
                .iter()
                .map(|node| {
                    let bounds = node.bounds.expect("painted nodes have bounds");
                    (
                        ((bounds[0] + bounds[2] / 2.0) * 10.0).round() as i64,
                        ((bounds[1] + bounds[3] / 2.0) * 10.0).round() as i64,
                    )
                })
                .collect();
            if positions.len() != painted.len() {
                return Err(format!(
                    "{} painted node(s) matching {:?} occupy only {} distinct position(s)",
                    painted.len(),
                    check.subject,
                    positions.len()
                ));
            }
        }
        Expect::ContainedBy => {
            let compare = check
                .compare
                .as_deref()
                .ok_or_else(|| "ContainedBy requires compare".to_owned())?;
            let painted: Vec<_> = found.iter().copied().filter(|node| paints(node)).collect();
            if painted.is_empty() {
                return Err(format!("no painted node matching {:?}", check.subject));
            }
            let container = matching(after, compare)
                .into_iter()
                .filter(|node| !painted.iter().any(|subject| subject.id == node.id))
                .find_map(|node| paints(node).then_some(node.bounds).flatten())
                .ok_or_else(|| format!("no painted comparison node matching {compare:?}"))?;
            const SLACK: f64 = 1.0;
            for node in painted {
                let bounds = node.bounds.expect("painted nodes have bounds");
                let inside = bounds[0] >= container[0] - SLACK
                    && bounds[1] >= container[1] - SLACK
                    && bounds[0] + bounds[2] <= container[0] + container[2] + SLACK
                    && bounds[1] + bounds[3] <= container[1] + container[3] + SLACK;
                if !inside {
                    return Err(format!(
                        "{:?} id={} at {:.0},{:.0} {:.0}x{:.0} escapes {compare:?} at \
                         {:.0},{:.0} {:.0}x{:.0}",
                        check.subject,
                        node.id,
                        bounds[0],
                        bounds[1],
                        bounds[2],
                        bounds[3],
                        container[0],
                        container[1],
                        container[2],
                        container[3]
                    ));
                }
            }
        }
        Expect::TargetPaints => {
            return Err("TargetPaints must be resolved by the live QA runner".to_owned());
        }
        Expect::Measures => {
            let want = check
                .expect_size
                .as_deref()
                .ok_or_else(|| "Measures requires expect_size".to_owned())?;
            let painted = found
                .iter()
                .copied()
                .filter(|node| paints(node))
                .collect::<Vec<_>>();
            if painted.is_empty() {
                return Err(format!("no painted node matching {:?}", check.subject));
            }
            let (want_w, want_h) = want.split_once('x').ok_or_else(|| {
                format!("expect_size {want:?} is not WxH; use `190x24`, `x24` or `190x`")
            })?;

            for node in painted {
                let box_ = node.bounds.expect("painted nodes have bounds");
                for (axis, spec, actual) in
                    [("width", want_w, box_[2]), ("height", want_h, box_[3])]
                {
                    let spec = spec.trim();
                    if spec.is_empty() {
                        continue;
                    }
                    // A pixel of slack, because layout rounds and a control that
                    // asks for 24 can legitimately resolve to 23.6.
                    const SLACK: f64 = 1.0;
                    let (compare, number) = match spec.strip_prefix("<=") {
                        Some(rest) => ("at most", rest),
                        None => match spec.strip_prefix('=') {
                            Some(rest) => ("exactly", rest),
                            None => ("at least", spec),
                        },
                    };
                    let target: f64 = number
                        .trim()
                        .parse()
                        .map_err(|_| format!("expect_size {axis} {number:?} is not a number"))?;
                    let ok = match compare {
                        "at most" => actual <= target + SLACK,
                        "exactly" => (actual - target).abs() <= SLACK,
                        _ => actual >= target - SLACK,
                    };
                    if !ok {
                        return Err(format!(
                            "{:?} id={} is {actual:.0}px {axis}, expected {compare} {target:.0}",
                            check.subject, node.id
                        ));
                    }
                }
            }
        }
        Expect::Above => {
            let compare = check
                .compare
                .as_deref()
                .ok_or_else(|| "Above requires compare".to_owned())?;
            if found.is_empty() {
                return Err(format!("no node matching {:?}", check.subject));
            }
            let others: Vec<_> = matching(after, compare)
                .into_iter()
                .filter(|node| paints(node))
                .collect();
            if others.is_empty() {
                return Err(format!("no painted comparison node matching {compare:?}"));
            }
            for subject_node in &found {
                if !paints(subject_node) {
                    return Err(format!(
                        "{:?} id={} does not paint",
                        check.subject, subject_node.id
                    ));
                }
                let subject = subject_node.bounds.expect("painted nodes have bounds");
                if !others.iter().any(|other| {
                    other.id != subject_node.id
                        && other.bounds.is_some_and(|bounds| subject[1] < bounds[1])
                }) {
                    return Err(format!(
                        "{:?} id={} is at y={:.0}, not above any {compare:?}",
                        check.subject, subject_node.id, subject[1]
                    ));
                }
            }
        }
        Expect::RightOf => {
            let compare = check
                .compare
                .as_deref()
                .ok_or_else(|| "RightOf requires compare".to_owned())?;
            if found.is_empty() {
                return Err(format!("no node matching {:?}", check.subject));
            }
            let others: Vec<_> = matching(after, compare)
                .into_iter()
                .filter(|node| paints(node))
                .collect();
            if others.is_empty() {
                return Err(format!("no painted comparison node matching {compare:?}"));
            }
            const SLACK: f64 = 1.0;
            for subject_node in &found {
                if !paints(subject_node) {
                    return Err(format!(
                        "{:?} id={} does not paint",
                        check.subject, subject_node.id
                    ));
                }
                let subject = subject_node.bounds.expect("painted nodes have bounds");
                if !others.iter().any(|other| {
                    other.id != subject_node.id
                        && other
                            .bounds
                            .is_some_and(|bounds| subject[0] >= bounds[0] + bounds[2] - SLACK)
                }) {
                    return Err(format!(
                        "{:?} id={} starts at x={:.0}, not right of any {compare:?}",
                        check.subject, subject_node.id, subject[0]
                    ));
                }
            }
        }
        Expect::CenterAlignedY => {
            let compare = check
                .compare
                .as_deref()
                .ok_or_else(|| "CenterAlignedY requires compare".to_owned())?;
            if found.is_empty() {
                return Err(format!("no node matching {:?}", check.subject));
            }
            let others: Vec<_> = matching(after, compare)
                .into_iter()
                .filter(|node| paints(node))
                .collect();
            if others.is_empty() {
                return Err(format!("no painted comparison node matching {compare:?}"));
            }
            const SLACK: f64 = 2.0;
            for subject_node in &found {
                if !paints(subject_node) {
                    return Err(format!(
                        "{:?} id={} does not paint",
                        check.subject, subject_node.id
                    ));
                }
                let subject = subject_node.bounds.expect("painted nodes have bounds");
                let subject_center = subject[1] + subject[3] / 2.0;
                if !others.iter().any(|other| {
                    other.id != subject_node.id
                        && other.bounds.is_some_and(|bounds| {
                            let other_center = bounds[1] + bounds[3] / 2.0;
                            (subject_center - other_center).abs() <= SLACK
                        })
                }) {
                    return Err(format!(
                        "{:?} id={} is centered at y={subject_center:.0}, not aligned with any {compare:?}",
                        check.subject, subject_node.id
                    ));
                }
            }
        }
        Expect::PixelsHold
        | Expect::PixelsHoldAfterHover
        | Expect::PixelsChange
        | Expect::VisibleInk
        | Expect::InteriorInk
        | Expect::OpaqueBackground
        | Expect::TransparentBackground
        | Expect::FullOpacity
        | Expect::TransparentWindowTint
        | Expect::Contrast
        | Expect::FontSizeGrows => {
            return Err("paint expectations must be resolved by the live QA runner".to_owned());
        }
        Expect::PaintsMore => {
            let was = matching(before, &check.subject)
                .into_iter()
                .filter(|node| paints(node))
                .count();
            let now = found.iter().filter(|node| paints(node)).count();
            if now <= was {
                return Err(format!(
                    "{:?} on screen went {was} -> {now}, expected one more",
                    check.subject
                ));
            }
        }
        Expect::FamilyChanges => {
            let family = |nodes: &[SemanticNode]| {
                matching(nodes, &check.subject)
                    .into_iter()
                    .filter(|node| paints(node))
                    .map(|node| (node.id, node.name.clone()))
                    .collect::<std::collections::HashSet<_>>()
            };
            let was = family(before);
            let now = family(after);
            if now.is_empty() {
                return Err(format!(
                    "no painted node matching {:?} after pagination",
                    check.subject
                ));
            }
            if now == was {
                return Err(format!(
                    "{:?} kept the same {} rendered member(s)",
                    check.subject,
                    now.len()
                ));
            }
        }
        Expect::Grows => {
            let was = matching(before, &check.subject).len();
            let now = found.len();
            if now <= was {
                return Err(format!(
                    "{:?} went {was} -> {now}, expected more",
                    check.subject
                ));
            }
        }
        Expect::Holds => {
            let was = matching(before, &check.subject).len();
            let now = found.len();
            if now != was {
                return Err(format!(
                    "{:?} went {was} -> {now}, expected no change",
                    check.subject
                ));
            }
        }
        Expect::Count => {
            let want = check
                .expect_count
                .ok_or_else(|| "Count requires expect_count".to_owned())?;
            if found.len() != want {
                return Err(format!(
                    "{:?} has {} member(s), expected {want}",
                    check.subject,
                    found.len()
                ));
            }
        }
        Expect::ValueChanges => {
            let before_node = matching(before, &check.subject)
                .into_iter()
                .find(|node| paints(node))
                .ok_or_else(|| {
                    format!("no painted node matching {:?} before action", check.subject)
                })?;
            value_changed(before_node.id, before, after)?;
        }
        Expect::SelectionChanges => {
            let before_node = matching(before, &check.subject)
                .into_iter()
                .find(|node| paints(node))
                .ok_or_else(|| {
                    format!("no painted node matching {:?} before action", check.subject)
                })?;
            selection_changed(before_node.id, before, after)?;
        }
        Expect::NameChanges => {
            let before_node = matching(before, &check.subject)
                .into_iter()
                .find(|node| paints(node))
                .ok_or_else(|| {
                    format!("no painted node matching {:?} before action", check.subject)
                })?;
            name_changed(before_node.id, before, after)?;
        }
    }
    Ok(())
}

/// Assert that one exact semantic node changed its exposed value.
///
/// The live runner uses this after a named action has already resolved to an
/// id. Re-resolving by name here would let a neighbouring same-name row decide
/// the verdict instead of the control that was actually activated.
pub fn value_changed(
    node_id: u64,
    before: &[SemanticNode],
    after: &[SemanticNode],
) -> Result<(), String> {
    let before_node = before
        .iter()
        .find(|node| node.id == node_id)
        .ok_or_else(|| format!("node {node_id} was absent before action"))?;
    let after_node = after
        .iter()
        .find(|node| node.id == node_id)
        .ok_or_else(|| format!("node {node_id} disappeared after action"))?;
    let old = before_node
        .value
        .as_deref()
        .ok_or_else(|| format!("node {node_id} has no semantic value before action"))?;
    let new = after_node
        .value
        .as_deref()
        .ok_or_else(|| format!("node {node_id} has no semantic value after action"))?;
    if old == new {
        return Err(format!("semantic value for node {node_id} stayed {old:?}"));
    }
    Ok(())
}

/// Assert that one exact semantic node changed selected/pressed state.
pub fn selection_changed(
    node_id: u64,
    before: &[SemanticNode],
    after: &[SemanticNode],
) -> Result<(), String> {
    let before_node = before
        .iter()
        .find(|node| node.id == node_id)
        .ok_or_else(|| format!("node {node_id} was absent before action"))?;
    let after_node = after
        .iter()
        .find(|node| node.id == node_id)
        .ok_or_else(|| format!("node {node_id} disappeared after action"))?;
    if before_node.selected == after_node.selected {
        return Err(format!(
            "selected state for node {node_id} stayed {}",
            before_node.selected
        ));
    }
    Ok(())
}

/// Assert that one exact semantic node changed its accessible name.
pub fn name_changed(
    node_id: u64,
    before: &[SemanticNode],
    after: &[SemanticNode],
) -> Result<(), String> {
    let before_node = before
        .iter()
        .find(|node| node.id == node_id)
        .ok_or_else(|| format!("node {node_id} was absent before action"))?;
    let after_node = after
        .iter()
        .find(|node| node.id == node_id)
        .ok_or_else(|| format!("node {node_id} disappeared after action"))?;
    if before_node.name == after_node.name {
        return Err(format!(
            "accessible name for node {node_id} stayed {:?}",
            before_node.name
        ));
    }
    Ok(())
}

/// Every check, grouped, with what it drives and what it asserts.
///
/// Printed by `ps-qa list`, and generated from [`checks`] rather than written
/// down, so it cannot drift from what actually runs. This is the inventory: it
/// answers "what is covered" without launching the app, which is the question
/// that had no answer while the audit was a list of button names in a handover.
pub fn manifest(dir: Option<&std::path::Path>) -> Result<String, String> {
    let all = checks(dir)?;
    let mut out = String::new();
    let mut current = String::new();
    for check in &all {
        if check.group != current {
            current = check.group.clone();
            out.push_str(&format!("\n{current}\n"));
        }
        let action = action_description(check);
        out.push_str(&format!(
            "  {:<26} {}\n{:<29}{} -> {:?} {:?}\n",
            check.id, check.what, "", action, check.expect, check.subject
        ));
        if !check.covers.is_empty() {
            out.push_str(&format!("{:<29}covers {:?}\n", "", check.covers));
        }
    }
    out.push_str(&format!("\n{} checks in {} groups\n", all.len(), {
        all.iter()
            .map(|check| check.group.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
    }));
    Ok(out)
}

fn action_description(check: &Check) -> String {
    let mut action = match (&check.hover, &check.click, check.press) {
        (Some(h), Some(c), true) => format!("hover {h:?}, press {c:?}"),
        (Some(h), Some(c), false) => format!("hover {h:?}, activate {c:?}"),
        (Some(h), None, _) => format!("hover {h:?}"),
        (None, Some(c), true) => format!("press {c:?}"),
        (None, Some(c), false) => format!("activate {c:?}"),
        (None, None, _) => "observe only".to_owned(),
    };
    if let Some(prepare) = check.prepare.as_deref() {
        let preparation = if let Some(key) = check.prepare_key.as_deref() {
            format!("prepare-key {key:?} on {prepare:?}")
        } else if check.prepare_press {
            format!("prepare-press {prepare:?}")
        } else {
            format!("prepare {prepare:?}")
        };
        action = format!("{preparation}, {action}");
    }
    if let (Some(field), Some(value)) = (
        check.setup_type_into.as_deref(),
        check.setup_text.as_deref(),
    ) {
        action = format!("setup {value:?} in {field:?}, {action}");
    }
    if let Some(field) = check.type_into.as_deref() {
        let typed = check.text.as_deref().map_or_else(
            || format!("focus {field:?}"),
            |value| format!("type {value:?} into {field:?}"),
        );
        if action == "observe only" {
            action = typed;
        } else {
            action.push_str(&format!(", {typed}"));
        }
    }
    if let (Some(key), Some(target)) = (
        &check.key,
        check.key_on.as_ref().or(check.type_into.as_ref()),
    ) {
        action.push_str(&format!(", key {key:?} on {target:?}"));
    }
    if let Some(target) = check.scroll_over.as_deref() {
        action.push_str(&format!(
            ", scroll {} x {:.0} over {target:?}",
            check.scroll_ticks, check.scroll_delta
        ));
    }
    action
}

/// Count matching nodes per group, for the summary line.
pub fn tally<'a>(results: &[(&'a Check, Result<(), String>)]) -> HashMap<&'a str, (usize, usize)> {
    let mut by_group: HashMap<&str, (usize, usize)> = HashMap::new();
    for (check, outcome) in results {
        let entry = by_group.entry(check.group.as_str()).or_insert((0, 0));
        entry.1 += 1;
        if outcome.is_ok() {
            entry.0 += 1;
        }
    }
    by_group
}

#[cfg(test)]
mod tests {
    use super::{
        Check, Expect, action_description, name_changed, selection_changed, validate_check,
        value_changed, verdict,
    };
    use blitz_control_protocol::SemanticNode;
    use std::collections::HashMap;
    use std::path::Path;

    fn parse(extra: &str) -> Check {
        let ron = format!(
            "(id:\"action\",group:\"group\",what:\"outcome\",open:None,hover:None,\
             click:Some(\"Save\"),{extra}subject:\"Saved\",expect:Paints)"
        );
        ron::from_str(&ron).expect("check parses")
    }

    fn painted_node(id: u64, name: &str, width: f64, height: f64) -> SemanticNode {
        SemanticNode {
            dom_id: None,
            id,
            parent: None,
            role: "generic".into(),
            name: name.into(),
            value: None,
            enabled: true,
            visible: true,
            selected: false,
            bounds: Some([0.0, 0.0, width, height]),
            slot: None,
        }
    }

    #[test]
    fn measures_rejects_any_matching_node_that_breaks_the_contract() {
        let mut check = parse("");
        check.click = None;
        check.expect = Expect::Measures;
        check.expect_size = Some("x<=24".into());

        let compact = painted_node(1, "Saved", 80.0, 24.0);
        let wrapped = painted_node(2, "Saved", 80.0, 52.0);
        let error = verdict(&check, &[], &[compact.clone(), wrapped])
            .expect_err("one valid representative cannot hide a wrapped sibling");
        assert!(error.contains("id=2"));
        assert!(error.contains("52px height"));

        assert!(verdict(&check, &[], &[compact.clone(), compact]).is_ok());
    }

    #[test]
    fn family_verdicts_reject_a_broken_sibling() {
        let good = painted_node(1, "Saved", 20.0, 20.0);
        let mut hidden = painted_node(2, "Saved", 20.0, 20.0);
        hidden.visible = false;
        hidden.bounds = Some([0.0, 0.0, 0.0, 0.0]);
        let mut check = parse("");
        check.click = None;
        check.expect = Expect::Paints;
        assert!(
            verdict(&check, &[], &[good.clone(), hidden]).is_err(),
            "one painted representative must not hide a broken sibling"
        );

        let comparison = SemanticNode {
            id: 9,
            name: "Anchor".into(),
            bounds: Some([100.0, 100.0, 20.0, 20.0]),
            ..painted_node(9, "Anchor", 20.0, 20.0)
        };
        check.compare = Some("Anchor".into());

        for (expect, valid_bounds, broken_bounds) in [
            (
                Expect::Above,
                [100.0, 70.0, 20.0, 20.0],
                [100.0, 130.0, 20.0, 20.0],
            ),
            (
                Expect::RightOf,
                [130.0, 100.0, 20.0, 20.0],
                [90.0, 100.0, 20.0, 20.0],
            ),
            (
                Expect::CenterAlignedY,
                [130.0, 100.0, 20.0, 20.0],
                [130.0, 130.0, 20.0, 20.0],
            ),
        ] {
            let mut valid = good.clone();
            valid.bounds = Some(valid_bounds);
            let mut broken = good.clone();
            broken.id = 2;
            broken.bounds = Some(broken_bounds);
            check.expect = expect;
            assert!(
                verdict(
                    &check,
                    &[],
                    &[valid.clone(), broken.clone(), comparison.clone()]
                )
                .is_err(),
                "{expect:?} must validate every matching subject"
            );
        }
    }

    #[test]
    fn checks_default_to_semantic_actions_and_can_opt_into_pointer_press() {
        assert!(!parse("").press);
        assert!(parse("press:true,").press);
    }

    #[test]
    fn destructive_checks_are_explicit_and_default_to_the_shared_body() {
        assert!(!parse("").destructive);
        assert!(parse("destructive:true,").destructive);
    }

    #[test]
    fn checks_can_prepare_a_semantic_state_before_the_measured_action() {
        let check = parse("prepare:Some(\"Draft\"),");
        assert_eq!(check.prepare.as_deref(), Some("Draft"));
        assert_eq!(
            action_description(&check),
            "prepare \"Draft\", activate \"Save\""
        );
    }

    #[test]
    fn checks_can_make_preparation_idempotent() {
        let check = parse("prepare:Some(\"Menu\"),prepare_unless:Some(\"menuitem:First\"),");
        assert_eq!(check.prepare.as_deref(), Some("Menu"));
        assert_eq!(check.prepare_unless.as_deref(), Some("menuitem:First"));
    }

    #[test]
    fn checks_can_make_hover_idempotent() {
        let check = parse("hover_unless:Some(\"Dialog\"),");
        assert_eq!(check.hover_unless.as_deref(), Some("Dialog"));
    }

    #[test]
    fn checks_can_prepare_with_a_real_pointer_without_changing_the_action_mode() {
        let check = parse("prepare:Some(\"Draft\"),prepare_press:true,");
        assert!(check.prepare_press);
        assert!(!check.press);
        assert_eq!(
            action_description(&check),
            "prepare-press \"Draft\", activate \"Save\""
        );
    }

    #[test]
    fn checks_can_prepare_with_a_key_without_changing_the_measured_action() {
        let check = parse("prepare:Some(\"Menu\"),prepare_key:Some(\"ArrowDown\"),");
        assert_eq!(check.prepare_key.as_deref(), Some("ArrowDown"));
        assert!(!check.press);
        assert_eq!(
            action_description(&check),
            "prepare-key \"ArrowDown\" on \"Menu\", activate \"Save\""
        );
    }

    #[test]
    fn duplicate_check_ids_are_rejected_before_a_run() {
        let check = parse("");
        let mut ids = HashMap::new();
        validate_check(&check, Path::new("first.ron"), &mut ids).expect("first id is unique");
        let error = validate_check(&check, Path::new("second.ron"), &mut ids)
            .expect_err("duplicate id must fail");
        assert!(error.contains("duplicate check id \"action\""));
        assert!(error.contains("first.ron"));
        assert!(error.contains("second.ron"));
    }

    #[test]
    fn a_dismissal_cannot_pass_by_sending_prepare_key_to_a_closed_subject() {
        let mut check = parse(
            "prepare:Some(\"Menu\"),prepare_unless:Some(\"menuitem:First\"),\
             prepare_key:Some(\"Escape\"),",
        );
        check.click = None;
        check.subject = "menuitem:First".into();
        check.expect = Expect::Vanishes;

        let error = validate_check(&check, Path::new("menu.ron"), &mut HashMap::new())
            .expect_err("closed-menu false green must fail validation");
        assert!(error.contains("can pass without opening"));
        assert!(error.contains("key and key_on"));
    }

    #[test]
    fn checks_can_repeat_hover_after_preparation() {
        let check = parse("after_prepare_hover:Some((\"menuitem:low\",5)),");
        let hover = check.after_prepare_hover.expect("post-prepare hover");
        assert_eq!(hover.target(), "menuitem:low");
        assert_eq!(hover.times(), 5);
    }

    #[test]
    fn checks_can_reveal_a_region_before_their_first_capture() {
        let check = parse("reveal_before_capture:Some(\"heading:Appearance\"),");
        assert_eq!(
            check.reveal_before_capture.as_deref(),
            Some("heading:Appearance")
        );
    }

    #[test]
    fn checks_can_establish_typed_state_before_the_measured_action() {
        let check = parse("setup_type_into:Some(\"Search projects\"),setup_text:Some(\"theta\"),");
        assert_eq!(
            action_description(&check),
            "setup \"theta\" in \"Search projects\", activate \"Save\""
        );
        validate_check(&check, Path::new("search.ron"), &mut HashMap::new())
            .expect("paired setup input is valid");
    }

    #[test]
    fn typed_setup_requires_both_the_field_and_value() {
        let check = parse("setup_type_into:Some(\"Search projects\"),");
        let error = validate_check(&check, Path::new("search.ron"), &mut HashMap::new())
            .expect_err("an incomplete setup must fail validation");
        assert!(error.contains("setup_type_into and setup_text together"));
    }

    #[test]
    fn checks_can_describe_literal_semantic_input() {
        let check = parse(
            "type_into:Some(\"New record\"),text:Some(\"latest fixture\"),\
             key:Some(\"Enter\"),compare:Some(\"older\"),\
             covers:[\"button:Save row \"],settle_after_ms:1200,",
        );
        assert_eq!(check.type_into.as_deref(), Some("New record"));
        assert_eq!(check.text.as_deref(), Some("latest fixture"));
        assert_eq!(check.key.as_deref(), Some("Enter"));
        assert_eq!(check.compare.as_deref(), Some("older"));
        assert_eq!(check.covers, ["button:Save row "]);
        assert_eq!(check.settle_after_ms, 1200);
        assert_eq!(check.stable_for_ms, 0);
        assert_eq!(
            action_description(&check),
            "activate \"Save\", type \"latest fixture\" into \"New record\", key \"Enter\" on \"New record\""
        );
    }

    #[test]
    fn checks_can_describe_real_wheel_input() {
        let check = parse("scroll_over:Some(\"listitem:\"),scroll_ticks:4,scroll_delta:-300.0,");
        assert_eq!(check.scroll_over.as_deref(), Some("listitem:"));
        assert_eq!(check.scroll_ticks, 4);
        assert_eq!(check.scroll_delta, -300.0);
        assert_eq!(
            action_description(&check),
            "activate \"Save\", scroll 4 x -300 over \"listitem:\""
        );
    }

    #[test]
    fn count_checks_require_an_exact_count() {
        let mut check = parse("");
        check.expect = Expect::Count;
        let error = validate_check(&check, Path::new("count.ron"), &mut HashMap::new())
            .expect_err("Count without an exact value is ambiguous");
        assert!(error.contains("expect_count"));

        check.expect_count = Some(12);
        validate_check(&check, Path::new("count.ron"), &mut HashMap::new())
            .expect("an exact count is valid");
    }

    #[test]
    fn a_value_check_follows_the_same_node_id() {
        let check = Check {
            id: "slider".into(),
            group: "settings".into(),
            what: "the slider moves".into(),
            open: None,
            prepare: None,
            prepare_unless: None,
            prepare_press: false,
            prepare_key: None,
            hover: None,
            hover_unless: None,
            after_prepare_hover: None,
            reveal_before_capture: None,
            setup_type_into: None,
            setup_text: None,
            click: None,
            type_into: None,
            text: None,
            key: Some("Right".into()),
            key_on: Some("Output level".into()),
            scroll_over: None,
            scroll_ticks: 0,
            scroll_delta: 0.0,
            compare: None,
            expect_size: None,
            expect_count: None,
            covers: Vec::new(),
            press: false,
            settle_after_ms: 0,
            outcome_timeout_ms: 0,
            stable_for_ms: 0,
            destructive: false,
            subject: "Output level".into(),
            expect: Expect::ValueChanges,
        };
        let node = |id, value: &str| SemanticNode {
            dom_id: None,
            id,
            parent: None,
            role: "slider".into(),
            name: "Output level".into(),
            value: Some(value.into()),
            enabled: true,
            visible: true,
            selected: false,
            bounds: Some([0.0, 0.0, 100.0, 20.0]),
            slot: None,
        };

        assert!(verdict(&check, &[node(7, "0")], &[node(7, "1")]).is_ok());
        assert!(verdict(&check, &[node(7, "0")], &[node(7, "0")]).is_err());
        assert!(
            verdict(&check, &[node(7, "0")], &[node(7, "0"), node(8, "1")],).is_err(),
            "a neighbouring repeated control cannot satisfy the check"
        );
        assert!(
            value_changed(
                8,
                &[node(7, "0"), node(8, "0")],
                &[node(7, "1"), node(8, "0")],
            )
            .is_err(),
            "the exact activated id cannot be replaced by a same-name neighbour"
        );
    }

    #[test]
    fn a_virtualized_family_can_advance_without_growing() {
        let node = |id, name: &str| SemanticNode {
            dom_id: None,
            id,
            parent: None,
            role: "button".into(),
            name: name.into(),
            value: None,
            enabled: true,
            visible: true,
            selected: false,
            bounds: Some([0.0, 0.0, 100.0, 24.0]),
            slot: None,
        };
        let first = node(1, "Rename project alpha");
        let mut second = node(2, "Rename project beta");
        let mut check = parse("");
        check.subject = "button:Rename project ".into();
        check.expect = Expect::FamilyChanges;

        assert!(
            verdict(
                &check,
                std::slice::from_ref(&first),
                std::slice::from_ref(&second)
            )
            .is_ok()
        );
        assert!(
            verdict(
                &check,
                std::slice::from_ref(&first),
                std::slice::from_ref(&first)
            )
            .is_err()
        );
        second.bounds = Some([0.0, 0.0, 0.0, 0.0]);
        assert!(verdict(&check, &[first], &[second]).is_err());
    }

    #[test]
    fn a_selection_check_follows_the_same_node_id() {
        let before = SemanticNode {
            dom_id: None,
            id: 7,
            parent: None,
            role: "radio".into(),
            name: "Theme colour".into(),
            value: None,
            enabled: true,
            visible: true,
            selected: false,
            bounds: Some([0.0, 0.0, 20.0, 20.0]),
            slot: None,
        };
        let mut after = before.clone();
        after.selected = true;
        assert!(
            selection_changed(
                7,
                std::slice::from_ref(&before),
                std::slice::from_ref(&after),
            )
            .is_ok()
        );
        assert!(
            selection_changed(
                7,
                std::slice::from_ref(&before),
                std::slice::from_ref(&before),
            )
            .is_err()
        );
    }

    #[test]
    fn a_name_check_follows_the_same_node_id() {
        let node = |id, name: &str| SemanticNode {
            dom_id: None,
            id,
            parent: None,
            role: "status".into(),
            name: name.into(),
            value: None,
            enabled: true,
            visible: true,
            selected: false,
            bounds: Some([0.0, 0.0, 100.0, 20.0]),
            slot: None,
        };

        assert!(name_changed(7, &[node(7, "Refreshed 1")], &[node(7, "Refreshed 2")]).is_ok());
        assert!(name_changed(7, &[node(7, "Refreshed 1")], &[node(7, "Refreshed 1")]).is_err());
        assert!(
            name_changed(
                7,
                &[node(7, "Refreshed 1")],
                &[node(7, "Refreshed 1"), node(8, "Refreshed 2")],
            )
            .is_err(),
            "a neighbouring status node cannot satisfy the check"
        );
    }

    #[test]
    fn a_named_paint_failure_reports_a_still_painted_activator() {
        let mut check = parse("");
        check.subject = "textbox:Rename project".into();
        check.expect = Expect::PaintsNamed;
        let node = |id, role: &str, bounds: Option<[f64; 4]>| SemanticNode {
            dom_id: None,
            id,
            parent: None,
            role: role.into(),
            name: "Rename project".into(),
            value: None,
            enabled: true,
            visible: bounds.is_some(),
            selected: false,
            bounds,
            slot: None,
        };
        let error = verdict(
            &check,
            &[],
            &[
                node(7, "textbox", Some([0.0, 0.0, 0.0, 0.0])),
                node(8, "button", Some([10.0, 10.0, 20.0, 20.0])),
            ],
        )
        .expect_err("the textbox does not paint");
        assert!(error.contains("other painted matches: button id=8"));
    }

    #[test]
    fn paints_named_uses_the_shared_selector_language() {
        let node = SemanticNode {
            dom_id: Some("project-rename".into()),
            id: 7,
            parent: None,
            role: "textbox".into(),
            name: "Rename project".into(),
            value: None,
            enabled: true,
            visible: true,
            selected: false,
            bounds: Some([10.0, 10.0, 100.0, 20.0]),
            slot: Some("inline-edit".into()),
        };
        let mut check = parse("");
        check.expect = Expect::PaintsNamed;

        for selector in [
            "TEXTBOX:*PROJECT",
            "rename proj*",
            "#project-rename",
            "@inline-edit",
        ] {
            check.subject = selector.into();
            verdict(&check, &[], std::slice::from_ref(&node)).unwrap_or_else(|error| {
                panic!("{selector:?} did not use shared semantics: {error}")
            });
        }
    }

    #[test]
    fn a_positioned_family_rejects_stacked_controls() {
        let mut check = parse("");
        check.subject = "radio:Theme color".into();
        check.expect = Expect::DistinctPositions;
        let node = |id, x, y| SemanticNode {
            dom_id: None,
            id,
            parent: None,
            role: "radio".into(),
            name: format!("Theme color {id}"),
            value: None,
            enabled: true,
            visible: true,
            selected: false,
            bounds: Some([x, y, 20.0, 20.0]),
            slot: None,
        };

        assert!(verdict(&check, &[], &[node(1, 10.0, 10.0), node(2, 40.0, 10.0)]).is_ok());
        let error = verdict(&check, &[], &[node(1, 10.0, 10.0), node(2, 10.0, 10.0)])
            .expect_err("stacked controls are not a positioned family");
        assert!(error.contains("only 1 distinct position"));
    }

    #[test]
    fn a_rendered_family_must_stay_inside_its_comparison_box() {
        let mut check = parse("compare:Some(\"group:Surface colour\"),");
        check.subject = "radio:Theme color".into();
        check.expect = Expect::ContainedBy;
        let child = |id, x, y| SemanticNode {
            dom_id: None,
            id,
            parent: Some(1),
            role: "radio".into(),
            name: format!("Theme color {id}"),
            value: None,
            enabled: true,
            visible: true,
            selected: false,
            bounds: Some([x, y, 20.0, 20.0]),
            slot: None,
        };
        let container = SemanticNode {
            dom_id: None,
            id: 1,
            parent: None,
            role: "group".into(),
            name: "Surface colour".into(),
            value: None,
            enabled: true,
            visible: true,
            selected: false,
            bounds: Some([10.0, 10.0, 190.0, 190.0]),
            slot: None,
        };

        assert!(
            verdict(
                &check,
                &[],
                &[
                    container.clone(),
                    child(2, 20.0, 20.0),
                    child(3, 160.0, 160.0)
                ]
            )
            .is_ok()
        );
        let error = verdict(
            &check,
            &[],
            &[container, child(2, 20.0, 20.0), child(3, 195.0, 195.0)],
        )
        .expect_err("a detached petal must fail containment");
        assert!(error.contains("escapes \"group:Surface colour\""));
    }

    #[test]
    fn vertical_center_alignment_compares_rendered_boxes() {
        let mut check = parse("compare:Some(\"@adjustments\"),");
        check.subject = "@color-wheel-flower".into();
        check.expect = Expect::CenterAlignedY;
        let node = |id, slot: &str, bounds| SemanticNode {
            dom_id: None,
            id,
            parent: None,
            role: "generic".into(),
            name: String::new(),
            value: None,
            enabled: true,
            visible: true,
            selected: false,
            bounds: Some(bounds),
            slot: Some(slot.into()),
        };

        let wheel = node(1, "color-wheel-flower", [10.0, 110.0, 190.0, 190.0]);
        let aligned = node(2, "adjustments", [220.0, 20.0, 400.0, 370.0]);
        assert!(verdict(&check, &[], &[wheel.clone(), aligned]).is_ok());

        let pinned_to_top = node(2, "adjustments", [220.0, 110.0, 400.0, 370.0]);
        let error = verdict(&check, &[], &[wheel, pinned_to_top])
            .expect_err("top-pinned wheel must fail vertical centering");
        assert!(error.contains("not aligned"));
    }

    #[test]
    fn enabled_requires_the_control_to_paint_and_accept_input() {
        let mut check = parse("");
        check.subject = "Save".into();
        check.expect = Expect::Enabled;
        let node = |enabled, bounds| SemanticNode {
            dom_id: None,
            id: 7,
            parent: None,
            role: "button".into(),
            name: "Save".into(),
            value: None,
            enabled,
            visible: true,
            selected: false,
            bounds,
            slot: None,
        };

        assert!(verdict(&check, &[], &[node(true, Some([0.0, 0.0, 20.0, 20.0]))]).is_ok());
        assert!(verdict(&check, &[], &[node(false, Some([0.0, 0.0, 20.0, 20.0]))]).is_err());
        assert!(verdict(&check, &[], &[node(true, Some([0.0, 0.0, 0.0, 0.0]))]).is_err());

        check.expect = Expect::Disabled;
        assert!(verdict(&check, &[], &[node(false, Some([0.0, 0.0, 20.0, 20.0]))]).is_ok());
        assert!(verdict(&check, &[], &[node(true, Some([0.0, 0.0, 20.0, 20.0]))]).is_err());
        assert!(verdict(&check, &[], &[node(false, Some([0.0, 0.0, 0.0, 0.0]))]).is_err());
    }

    #[test]
    fn verdict_subjects_honor_role_qualified_names() {
        let mut check = parse("");
        check.subject = "button:Send".into();
        check.expect = Expect::Disabled;
        let node = |role: &str| SemanticNode {
            dom_id: None,
            id: 7,
            parent: None,
            role: role.into(),
            name: "Send".into(),
            value: None,
            enabled: false,
            visible: true,
            selected: false,
            bounds: Some([0.0, 0.0, 20.0, 20.0]),
            slot: None,
        };

        assert!(verdict(&check, &[], &[node("button")]).is_ok());
        assert!(verdict(&check, &[], &[node("textbox")]).is_err());
    }
}
