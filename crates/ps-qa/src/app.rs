//! What the harness has to be told about the application it is driving.
//!
//! # Why this exists
//!
//! `ps-qa` is a harness, not a test suite. A browser driver does not know what
//! an application's activity panel is, and neither should this: surface names,
//! collapsible sections, the region its transcript scrolls inside, are facts
//! about *that* application and belong with it.
//!
//! They were hardcoded in the harness. `reach.rs` knew one product's three
//! surface names; `folds_a_section` listed that product's six section headers;
//! the scroll commands defaulted to its transcript region. Pointing this binary
//! at a second application meant editing the harness, which is the definition
//! of the wrong seam.
//!
//! # How an application describes itself
//!
//! A RON file, found by `--app <path>` and falling back to `ps-qa.ron` beside
//! the checks. Data rather than code, because the
//! application that owns these names does not want to depend on this crate to
//! state them, and because a name is not worth a recompile.
//!
//! ```ron
//! AppProfile(
//!     surfaces: [
//!         (name: "preferences", opener: "Preferences"),
//!         (name: "dashboard",   opener: "Dashboard"),
//!     ],
//!     permanent_surfaces: ["Dashboard", "Preferences"],
//!     navigation_controls: ["Open Preferences"],
//!     sections: ["Records", "Activity"],
//!     document_row_markers: [" open \u{b7} "],
//!     document_openers: ["QA document"],
//!     pagination_controls: [" more records"],
//!     close_prefixes: ["Close "],
//!     dismiss_controls: ["Leave dialog"],
//!     row_action_prefixes: ["Rename "],
//!     fold_prefixes: ["Collapse ", "Hide "],
//!     deferred_controls: ["Start setup"],
//!     isolated_controls: ["Restart application"],
//!     inert_controls: ["Synchronize"],
//!     transcript_region: Some("Message history"),
//!     home_opener: Some("Dashboard"),
//! )
//! ```
//!
//! Every field is optional. One an application does not set is simply a rule the
//! harness does not apply.
//!
//! # The default names no product
//!
//! [`AppProfile::default`] is deliberately empty apart from the structural
//! pieces every Blitz application has. An application that ships no profile
//! gets a harness that sweeps what it can find and says it found no configured
//! surfaces, rather than one quietly hunting for another product's buttons.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A surface the sweep must visit, named by the control that opens it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceSpec {
    /// Shown in the report.
    pub name: String,
    /// The accessible name of the control that navigates here.
    ///
    /// [`DYNAMIC_DOCUMENT`](crate::reach::DYNAMIC_DOCUMENT) stands in for the
    /// first user-named document, resolved at run time when fixture names are
    /// not stable.
    pub opener: String,
    /// A control unique to this surface, used to prove it is in front and to
    /// scope coverage to its semantic subtree.
    #[serde(default)]
    pub marker: Option<String>,
    /// A text field whose query causes this surface to mount deferred rows.
    ///
    /// ps-qa writes a temporary query and clears it immediately. The
    /// application owns both the field name and the reveal policy.
    #[serde(default)]
    pub reveal_with: Option<String>,
}

/// A control reserved for the manual release pass, and what it opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualControl {
    /// The control's accessible name.
    pub label: String,
    /// What the application calls the thing it opens, for the worklist.
    pub command: String,
}

/// Everything about the application under test that the harness cannot infer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppProfile {
    /// Top-level surfaces, in the order they are swept.
    pub surfaces: Vec<SurfaceSpec>,
    /// Tabs that are always present and are never a document.
    ///
    /// Used to tell a permanent surface from an opened one when reading the tab
    /// strip. Some applications double a tab's label in its accessible name
    /// (`DashboardDashboard`), so both forms are accepted.
    pub permanent_surfaces: Vec<String>,
    /// Other controls that leave their current surface.
    ///
    /// These are exercised as surface openers, not in the middle of a plan
    /// whose remaining controls would disappear after navigation.
    pub navigation_controls: Vec<String>,
    /// Collapsible section headers, by their label without the count.
    ///
    /// A collapse takes everything under it off screen, so these are pressed
    /// last. Getting this list wrong is expensive: folding a section a dozen
    /// controls before the sweep reaches its rows makes every one of them read
    /// as vanished.
    pub sections: Vec<String>,
    /// The scrollable region a transcript lives in, if the application has one.
    pub transcript_region: Option<String>,
    /// The control that returns to the application's root surface.
    ///
    /// Used to recover when a sweep has walked somewhere it cannot get back
    /// from. Without it the harness simply reports the surface as unreachable.
    pub home_opener: Option<String>,
    /// Substrings that identify a document row in a list.
    ///
    /// A document's own name is not matchable: it is user data, and in a
    /// scrubbed QA profile it differs per build. What is stable is the summary
    /// the list renders beside it, so an application states the fragments of
    /// that summary instead - a count, an age, a path. An application that
    /// ships none simply has no row fallback.
    pub document_row_markers: Vec<String>,
    /// Exact semantic names that open a dynamic document in this QA profile.
    ///
    /// Unlike row markers these are deterministic fixture data. Declaring
    /// them lets an outcome suite recognize that it is already on the same
    /// document instead of returning Home and reopening it before every check.
    pub document_openers: Vec<String>,
    /// Name fragments for controls that reveal another page of the same list.
    ///
    /// Inventory activates these until none remain before it counts component
    /// instances. The application keeps ownership of the words, and the
    /// harness never guesses from a generic verb such as `Show`.
    pub pagination_controls: Vec<String>,
    /// Prefixes of controls that close a surface, e.g. `Close `.
    ///
    /// These retire the pane everything else stands on, so they are swept last.
    /// A prefix rather than a whole name because the label carries the subject:
    /// `Close <document>`.
    pub close_prefixes: Vec<String>,
    /// Accessible names of controls that dismiss an in-app dialog.
    ///
    /// The harness detects a modal structurally from its semantic role. Names
    /// remain application data because products and languages call their exit
    /// actions different things. Order is preference order when a dialog
    /// offers more than one way out.
    pub dismiss_controls: Vec<String>,
    /// Prefixes of controls that act on a row without opening it, e.g. `Rename `.
    ///
    /// Excluded when looking for the control that *opens* a document, because a
    /// row's rename pencil sits beside its opener and matches the same row name.
    pub row_action_prefixes: Vec<String>,
    /// Prefixes of controls that hide or fold a region, e.g. `Collapse `.
    ///
    /// Matched case-insensitively. Swept last for the same reason a section
    /// header is: pressing one takes everything beneath it off screen.
    pub fold_prefixes: Vec<String>,
    /// Controls that cannot safely run in an unattended renderer audit.
    ///
    /// A native file panel is not in the tree, no synthesised click or key
    /// reaches it, and opening one unattended leaves a run stuck behind a
    /// window it cannot dismiss. These are counted and named in the report
    /// rather than pressed, so the exceptions stay visible instead of becoming
    /// controls nobody remembers to test by hand.
    ///
    /// `label` is the control's accessible name; `command` is whatever the
    /// application wants printed beside it in the worklist.
    #[serde(alias = "native_choosers")]
    pub manual_controls: Vec<ManualControl>,
    /// Controls that must be pressed last, beyond the collapsible sections.
    ///
    /// A control that opens a new document navigates away and takes the rest of
    /// the plan with it, exactly as a collapse does. A "new document" control
    /// is the usual example; an application will have its own or none.
    pub deferred_controls: Vec<String>,
    /// Controls that end the current automation session or irreversibly reset it.
    ///
    /// These cannot participate in a shared broad sweep: a successful restart
    /// closes the very control socket the sweep needs for its next assertion,
    /// while reset or sign-out changes the fixture every later control stands
    /// on. They are counted and named as requiring an isolated outcome check,
    /// never hidden in the manual native-dialog bucket and never reported as
    /// swept merely because their click was acknowledged.
    pub isolated_controls: Vec<String>,
    /// Controls whose successful effect is outside the semantic tree.
    ///
    /// Matched as accessible-name prefixes. A clipboard write or backend
    /// refresh may legitimately leave the rendered component tree unchanged;
    /// the application must declare that weaker contract explicitly rather
    /// than teaching the harness product labels.
    pub inert_controls: Vec<String>,
}

impl AppProfile {
    /// Read a profile, or return an empty one if the path does not exist.
    ///
    /// A missing file is not an error: the diagnostic commands (`layout`,
    /// `dom`, `paint`) work against any Blitz application with no profile at
    /// all. Only the sweep needs to be told where to go.
    pub fn load(path: Option<&Path>) -> Result<Self, String> {
        let Some(path) = path.map(Path::to_path_buf).or_else(discover) else {
            return Err(
                "no application profile. Pass --app <path>, or put ps-qa.ron in the \
                 working directory: the harness knows nothing about any application \
                 without one, and guessing produces numbers measured against an \
                 application that does not exist."
                    .to_owned(),
            );
        };
        if !path.exists() {
            return Err(format!(
                "no application profile at {}. Pass --app <path>, or put ps-qa.ron in \
                 the working directory.",
                path.display()
            ));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        ron::from_str(&text).map_err(|error| format!("could not parse {}: {error}", path.display()))
    }

    /// Whether this exact accessible name dismisses an in-app dialog.
    pub fn dismisses_dialog(&self, name: &str) -> bool {
        self.dismiss_controls
            .iter()
            .any(|control| name.eq_ignore_ascii_case(control))
    }

    /// Whether a name is a section header, with or without its count.
    ///
    /// The header carries a count, so what follows the label must be a number
    /// or nothing: `Records` matches, `Records1` matches, and `Record sorting`
    /// does not.
    pub fn folds_a_section(&self, name: &str) -> bool {
        self.sections.iter().any(|section| {
            name.strip_prefix(section.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit()))
        })
    }

    /// Whether a name is a permanent surface rather than a document.
    ///
    /// Accepts the doubled form, because a tab's accessible name may repeat its
    /// label.
    pub fn is_permanent(&self, name: &str) -> bool {
        self.permanent_surfaces.iter().any(|surface| {
            name == surface.as_str() || name == format!("{surface}{surface}").as_str()
        })
    }
}

/// `--app <path>`, then `ps-qa.ron` in the working directory.
fn discover() -> Option<PathBuf> {
    if let Some(pinned) = crate::cli::app_profile() {
        return Some(pinned);
    }
    let beside = PathBuf::from("ps-qa.ron");
    beside.exists().then_some(beside)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing profile is an error, not an empty one.
    ///
    /// This test used to assert the opposite. Loading a default meant the
    /// harness carried on describing an application nobody had described to it,
    /// and every count it reported afterwards was measured against a surface
    /// list that did not exist. Refusing to start names the missing file
    /// instead.
    #[test]
    fn an_absent_profile_is_an_error() {
        let error = AppProfile::load(Some(Path::new("/nonexistent/ps-qa.ron")))
            .expect_err("a missing profile must not load a default");
        assert!(error.contains("no application profile"), "{error}");
        assert!(error.contains("--app"), "{error}");
    }

    #[test]
    fn a_section_matches_with_or_without_its_count() {
        let profile = AppProfile {
            sections: vec!["Records".to_owned(), "Activity".to_owned()],
            ..Default::default()
        };
        assert!(profile.folds_a_section("Records"));
        assert!(profile.folds_a_section("Records12"));
        assert!(profile.folds_a_section("Activity"));
        // The trap this guards: a control whose name merely starts the same way.
        assert!(!profile.folds_a_section("Record sorting"));
        assert!(!profile.folds_a_section("Recorder"));
    }

    #[test]
    fn a_permanent_surface_is_recognised_doubled() {
        let profile = AppProfile {
            permanent_surfaces: vec!["Dashboard".to_owned(), "Preferences".to_owned()],
            ..Default::default()
        };
        assert!(profile.is_permanent("Dashboard"));
        assert!(profile.is_permanent("DashboardDashboard"));
        assert!(!profile.is_permanent("Dashboard item"));
        assert!(!profile.is_permanent("some document"));
    }

    #[test]
    fn a_profile_round_trips_through_ron() {
        let profile = AppProfile {
            surfaces: vec![SurfaceSpec {
                name: "dashboard".to_owned(),
                opener: "Dashboard".to_owned(),
                marker: Some("Overview heading".to_owned()),
                reveal_with: Some("Search dashboard".to_owned()),
            }],
            permanent_surfaces: vec!["Dashboard".to_owned()],
            navigation_controls: vec!["Open Preferences".to_owned()],
            sections: vec!["Records".to_owned()],
            transcript_region: Some("Message history".to_owned()),
            home_opener: Some("Dashboard".to_owned()),
            deferred_controls: vec!["Create document".to_owned()],
            isolated_controls: vec!["Restart application".to_owned()],
            inert_controls: vec!["Synchronize".to_owned()],
            document_row_markers: vec![" open · ".to_owned()],
            document_openers: vec!["QA document".to_owned()],
            pagination_controls: vec![" more records".to_owned()],
            close_prefixes: vec!["Close ".to_owned()],
            dismiss_controls: vec!["Leave dialog".to_owned()],
            row_action_prefixes: vec!["Rename ".to_owned()],
            fold_prefixes: vec!["Collapse ".to_owned()],
            manual_controls: vec![ManualControl {
                label: "Import data".to_owned(),
                command: "open_native_picker".to_owned(),
            }],
        };
        let text = ron::to_string(&profile).expect("serialises");
        let back: AppProfile = ron::from_str(&text).expect("parses");
        assert_eq!(back.surfaces.len(), 1);
        assert_eq!(back.sections, vec!["Records".to_owned()]);
        assert_eq!(back.transcript_region.as_deref(), Some("Message history"));
        assert_eq!(back.document_openers, vec!["QA document".to_owned()]);
        assert_eq!(back.pagination_controls, vec![" more records".to_owned()]);
        assert!(back.dismisses_dialog("leave dialog"));
    }
}
