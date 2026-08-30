//! Typed local debugging protocol carried by endpoint-libs framing.
//!
//! This is deliberately not WebDriver. It models the native renderer and app
//! lifecycle directly, supports observation by more than one client, and has
//! no authentication handshake. Agent control binds only to a local transport;
//! expensive diagnostics collection remains an explicit compile-time feature
//! *of the server*, `tauri-runtime-blitz/diagnostics`.
//!
//! # Why this is its own crate
//!
//! These are the server's own definitions, and a client that hand-rolls the
//! JSON gets them wrong in ways that do not look like encoding mistakes.
//! `AgentAction` is adjacently tagged, so `{"action":"click","node_id":9}` is
//! malformed where `{"action":"click","params":{"node_id":9}}` is correct, and
//! the difference used to present as a hung application. Sharing the types
//! makes that a compile error.
//!
//! Speaking the protocol must not cost a renderer. `tauri-runtime-blitz` pulls
//! in tauri, winit, wgpu and blitz, so a measurement tool that depended on it
//! for these types would build a browser engine to send a wheel event. Nothing
//! here needs any of that: it is serde plus endpoint-libs' framing.
//!
//! # No `diagnostics` feature, on purpose
//!
//! The types are inert data definitions and cost nothing to compile, whereas
//! gating them creates feature skew: cargo unifies features across a build, so
//! one workspace member asking for diagnostics types while the server crate is
//! built without them turned `DebugResponse` into a non-exhaustive match in
//! code nobody had touched. What stays gated in the server is *collection*,
//! which is where the expense actually is. A build that cannot serve
//! diagnostics says so by omitting the tool from `tools/list`; see
//! [`encode_tools_list_response`].

use endpoint_libs::libs::ws::mcp_wire::{INVALID_PARAMS, INVALID_REQUEST, parse};
pub use endpoint_libs::libs::ws::{
    MessageStream, WireMessage,
    mcp_wire::{
        JsonRpcError, JsonRpcId, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
        MCP_PROTOCOL_VERSION,
    },
    transport::{TransportStream, framed_json},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const DEBUG_PROTOCOL_VERSION: u32 = 1;
pub const MCP_INITIALIZE: &str = "initialize";
pub const MCP_INITIALIZED_NOTIFICATION: &str = "notifications/initialized";
pub const MCP_TOOLS_LIST: &str = "tools/list";
pub const MCP_TOOLS_CALL: &str = "tools/call";
pub const AGENT_CONTROL_TOOL: &str = "blitz.agent.control";
pub const AGENT_EVENT_NOTIFICATION: &str = "notifications/blitz/agent";
pub const DIAGNOSTICS_TOOL: &str = "blitz.diagnostics";
pub const DIAGNOSTICS_EVENT_NOTIFICATION: &str = "notifications/blitz/diagnostics";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DebugDescriptor {
    pub protocol_version: u32,
    pub pid: u32,
    pub instance_id: String,
    pub address: String,
    pub renderer: String,
    pub renderer_revision: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IncomingRequest {
    Initialize {
        id: JsonRpcId,
    },
    Initialized,
    ToolsList {
        id: JsonRpcId,
    },
    Agent {
        id: JsonRpcId,
        request: AgentControlRequest,
    },
    Diagnostics {
        id: JsonRpcId,
        request: DiagnosticsRequest,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "command", content = "params", rename_all = "camelCase")]
pub enum AgentControlRequest {
    Inspect { root: Option<u64>, max_depth: u32 },
    Act(AgentAction),
    Relaunch,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "command", content = "params", rename_all = "camelCase")]
pub enum DiagnosticsRequest {
    Observe {
        streams: Vec<DebugStream>,
    },
    Snapshot(SnapshotRequest),
    Metrics,
    WaitForIdle,
    /// The native window layers which sit outside the renderer's document.
    ///
    /// A renderer capture cannot see whether an opaque platform window or a
    /// tinted native glass view was placed behind its transparent root. This
    /// response makes that compositor boundary observable without pretending
    /// pixels from an unknown desktop prove which native layer produced them.
    WindowComposition,
    /// Render the current document to an RGBA8 image and return the pixels.
    ///
    /// The one question the rest of this protocol cannot answer. A snapshot
    /// reports the boxes and colours the engine *resolved*, which is not the
    /// same as what it drew: an element can hold a correct box, a correct
    /// computed colour and an accessible role while painting nothing at all.
    /// That gap is not hypothetical. An entire icon set rendered blank in a
    /// shipping application while every snapshot, every layout box and every
    /// accessibility node read exactly right, and no tool in this protocol
    /// could see it.
    ///
    /// Rendered offscreen rather than read back from the window surface, which
    /// is deliberate and is what makes this portable: there is no swapchain to
    /// capture, no compositor to ask, and no display server to require. The
    /// same call works over SSH, in CI, on a headless Linux box and on Android.
    Capture(CaptureRequest),
}

/// What to draw, and how large.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CaptureRequest {
    /// The subtree to draw, or the whole document when absent.
    ///
    /// Capturing one node is the difference between a test that says "the
    /// window is not blank" and one that says "this button's icon is drawn",
    /// and the second is the one worth writing.
    pub node_id: Option<u64>,
    /// Pixel scale, so a small control can be inspected at a useful size.
    ///
    /// A 16px icon is a handful of pixels at 1x, and antialiasing dominates
    /// them; asking for it at 4x makes "is anything there" answerable without
    /// making the threshold a guess.
    pub scale: f32,
}

impl Default for CaptureRequest {
    fn default() -> Self {
        Self {
            node_id: None,
            scale: 1.0,
        }
    }
}

/// An RGBA8 image, as the renderer actually drew it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    /// Row-major RGBA8, four bytes per pixel, base64 over the wire.
    ///
    /// Base64 rather than raw bytes because the transport is line-delimited
    /// JSON, and a full window at 2x is a few megabytes: large enough to be
    /// worth encoding compactly, not large enough to justify a second channel.
    pub rgba_base64: String,
    /// The node this shows, when the request named one.
    pub node_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "result", content = "value", rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)] // wire shape is public; boxing would break consumers
pub enum DebugResponse {
    Ack,
    AgentSnapshot(AgentSnapshot),
    Snapshot(DebugSnapshot),
    Metrics(RendererMetrics),
    Idle(RevisionSet),
    WindowComposition(WindowComposition),
    Captured(CapturedImage),
    Error(DebugError),
}

/// Effective native composition selected for the application window.
///
/// `supported` is false for a headless host or a runtime which has no native
/// window integration. The remaining fields describe the path the runtime
/// actually installed, not merely values requested by application CSS.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WindowComposition {
    pub supported: bool,
    pub surface_transparent: bool,
    pub glass_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glass_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tint_rgba: Option<[u8; 4]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "event", content = "value", rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)] // wire shape is public; boxing would break consumers
pub enum DebugEvent {
    Snapshot(DebugSnapshot),
    Metrics(RendererMetrics),
    Console(ConsoleEntry),
    RuntimeError(RuntimeErrorEntry),
    /// A real window frame completed after style, layout and paint.
    ///
    /// The event carries only a monotonic revision. Consumers pull a snapshot
    /// or capture if they need one; pushing images or trees on every frame
    /// would turn observation into an unbounded second renderer workload.
    PaintCommitted {
        revision: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "event", content = "value", rename_all = "camelCase")]
pub enum AgentControlEvent {
    Ready(DebugDescriptor),
    TreeChanged { revision: u64 },
    Lifecycle(LifecycleEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DebugStream {
    Snapshots,
    Metrics,
    Console,
    RuntimeErrors,
    Paint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRequest {
    pub include_dom: bool,
    pub include_layout: bool,
    pub include_computed_style: bool,
    /// Restrict DOM, layout, and computed-style collection to these nodes.
    /// Empty preserves the full-snapshot behavior used by inspection reports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub node_ids: Vec<u64>,
}

/// A node's live border box, encoded on the wire as `[x, y, width, height]`.
///
/// Named fields keep consumers out of positional indexing while the custom
/// array conversion preserves compatibility with existing inspectors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(from = "[f64; 4]", into = "[f64; 4]")]
pub struct LayoutBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl From<[f64; 4]> for LayoutBounds {
    fn from([x, y, width, height]: [f64; 4]) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

impl From<LayoutBounds> for [f64; 4] {
    fn from(bounds: LayoutBounds) -> Self {
        [bounds.x, bounds.y, bounds.width, bounds.height]
    }
}

/// A two-dimensional layout size, encoded as `[width, height]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(from = "[f64; 2]", into = "[f64; 2]")]
pub struct LayoutSize {
    pub width: f64,
    pub height: f64,
}

impl From<[f64; 2]> for LayoutSize {
    fn from([width, height]: [f64; 2]) -> Self {
        Self { width, height }
    }
}

impl From<LayoutSize> for [f64; 2] {
    fn from(size: LayoutSize) -> Self {
        [size.width, size.height]
    }
}

/// A live scroll offset, encoded as `[x, y]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(from = "[f64; 2]", into = "[f64; 2]")]
pub struct LayoutOffset {
    pub x: f64,
    pub y: f64,
}

impl From<[f64; 2]> for LayoutOffset {
    fn from([x, y]: [f64; 2]) -> Self {
        Self { x, y }
    }
}

impl From<LayoutOffset> for [f64; 2] {
    fn from(offset: LayoutOffset) -> Self {
        [offset.x, offset.y]
    }
}

/// Computed CSS edges, encoded in shorthand order `[top, right, bottom, left]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(from = "[f64; 4]", into = "[f64; 4]")]
pub struct LayoutEdges {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

impl From<[f64; 4]> for LayoutEdges {
    fn from([top, right, bottom, left]: [f64; 4]) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }
}

impl From<LayoutEdges> for [f64; 4] {
    fn from(edges: LayoutEdges) -> Self {
        [edges.top, edges.right, edges.bottom, edges.left]
    }
}

/// One node's renderer-computed geometry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LayoutDiagnosticRow {
    pub node_id: u64,
    pub bounds: LayoutBounds,
    pub scroll_offset: LayoutOffset,
    pub client_size: LayoutSize,
    pub scroll_size: LayoutSize,
    pub scroll_range: LayoutSize,
    pub border: LayoutEdges,
    pub padding: LayoutEdges,
    pub content_size: LayoutSize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", content = "params", rename_all = "camelCase")]
pub enum AgentAction {
    /// Give one semantic node keyboard focus without activating it.
    ///
    /// Keyboard setup must not be implemented as a click: focusing a submit,
    /// delete or fork button by clicking it performs the action before the key
    /// under test is ever delivered.
    Focus {
        node_id: u64,
    },
    /// Activate one semantic node without resolving it back through screen
    /// coordinates. The runtime dispatches pointer, mouse and click events in
    /// browser order directly to this target.
    Click {
        node_id: u64,
    },
    /// Two activations in one runtime call, inside the platform's double-click
    /// interval. Used for rows whose single click only selects or folds them.
    DoubleClick {
        node_id: u64,
    },
    /// Present one semantic node as hovered. The target is still named by id;
    /// the runtime owns any renderer-specific positioning needed to update CSS
    /// hover state.
    Hover {
        node_id: u64,
    },
    SetValue {
        node_id: u64,
        value: String,
    },
    ScrollIntoView {
        node_id: u64,
    },
    /// Scroll a specific node's nearest scroll container by a delta.
    ///
    /// Wheel events carry no coordinates and are delivered to whatever the
    /// document last saw hovered, which an injected pointer move does not
    /// reliably set. That made every scrollable panel in the application
    /// undrivable from outside, so a bug that only appears further down a long
    /// page could not be reproduced without a human at the keyboard.
    ScrollBy {
        node_id: u64,
        delta_x: f64,
        delta_y: f64,
    },
    Input(InputCommand),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "input", rename_all = "camelCase")]
pub enum InputCommand {
    Key {
        phase: KeyPhase,
        key: String,
        code: String,
        modifiers: Modifiers,
    },
    Pointer {
        phase: PointerPhase,
        x: f64,
        y: f64,
        button: u16,
        modifiers: Modifiers,
    },
    Wheel {
        delta_x: f64,
        delta_y: f64,
        phase: WheelPhase,
        modifiers: Modifiers,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub meta: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum KeyPhase {
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PointerPhase {
    Move,
    Down,
    Up,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum WheelPhase {
    Started,
    Moved,
    Ended,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RevisionSet {
    pub document: u64,
    /// Blitz does not version style, layout and paint separately from the
    /// document, so these stay zero. Filling them with a copy of `document` would
    /// suggest a per-stage change signal the runtime cannot actually provide.
    pub style: u64,
    pub layout: u64,
    pub paint: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DebugSnapshot {
    pub revisions: RevisionSet,
    pub active_window: Option<String>,
    pub active_element: Option<u64>,
    pub dom: Option<serde_json::Value>,
    pub layout: Option<Vec<LayoutDiagnosticRow>>,
    /// Resolved colours per node, when `include_computed_style` was set.
    ///
    /// Only the properties that decide legibility. A full longhand dump for
    /// every node is megabytes of JSON nobody reads, and these are what a
    /// question about invisible text actually needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computed_style: Option<serde_json::Value>,
    pub metrics: RendererMetrics,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentSnapshot {
    pub revision: u64,
    pub active_window: Option<String>,
    pub focused_node: Option<u64>,
    pub nodes: Vec<SemanticNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SemanticNode {
    /// The stable DOM identity supplied by the application, when present.
    ///
    /// This is deliberately separate from `id`: `id` is Blitz's ephemeral
    /// node handle and may change whenever the element remounts. QA selectors
    /// need the authored HTML `id` so a test keeps targeting the same control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dom_id: Option<String>,
    pub id: u64,
    pub parent: Option<u64>,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub enabled: bool,
    pub visible: bool,
    pub selected: bool,
    pub bounds: Option<[f64; 4]>,
    /// The library slot this element is, when it is part of a component.
    ///
    /// Read from `data-slot`, which a component library emits to name its own
    /// parts. Addressing a control by the slot it *is* rather than by the text
    /// it happens to carry is what lets a harness tell a trigger from the thing
    /// it opens: those two routinely share an accessible name, so a check
    /// written against the name passes whether or not anything happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RendererMetrics {
    pub revisions: RevisionSet,
    /// Depth of a pending-work queue in front of the renderer.
    ///
    /// Blitz has no such queue: `View::redraw` resolves, paints and presents
    /// inline on the event-loop thread. The one queue that exists, `ScriptQueue`,
    /// lives inside the Tauri webview dispatcher and is not reachable from the
    /// runtime, so this stays `None` rather than reporting a constant zero that
    /// reads like a measured "nothing is backed up".
    pub queue_depth: Option<u64>,
    pub invalidations_coalesced: u64,
    /// The most recently presented frame, as measured by blitz-shell.
    ///
    /// `None` until the window has drawn at least once.
    pub frame: Option<FrameMetrics>,
    /// Mean, p95 and worst case across the last few hundred presented frames.
    pub frame_window: Option<FrameWindowMetrics>,
    /// Cost of collecting this diagnostic snapshot. This measures the observer,
    /// not the application, and is reported separately so the two are never
    /// confused again.
    pub snapshot: Option<SnapshotCost>,
    /// What JavaScript cost, per `ScriptDocument::poll`.
    ///
    /// The frame numbers above cover resolve, paint and present, which is the
    /// engine. Reactivity, event handlers, timers and microtasks are none of
    /// those, so an application could present a 4ms frame and still feel slow
    /// with nothing here to explain it. `None` until script has actually run.
    pub script: Option<ScriptMetrics>,
    pub resident_bytes: Option<u64>,
}

/// Script execution cost, in milliseconds, over the retained poll window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScriptMetrics {
    pub mean_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    /// Polls that ran script, within the retained window.
    pub window_polls: u64,
    /// Every poll since launch, including the ones that found nothing to do.
    pub total_polls: u64,
    /// Polls that ran script since launch.
    pub productive_polls: u64,
    /// Total time spent in the script runtime since launch.
    pub spent_ms: f64,
    /// Where that time went, worst total first: event name or timer source,
    /// call count, total milliseconds, worst single call.
    pub breakdown: Vec<ScriptSource>,
}

/// One attributed source of script time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScriptSource {
    pub label: String,
    pub calls: u64,
    pub total_ms: f64,
    pub worst_ms: f64,
}

/// Timings of one real presented frame, in milliseconds.
///
/// Every `Option` here is a value blitz does not measure. They are left empty on
/// purpose: a plausible-looking zero is worse than an admitted gap, because it
/// makes a missing measurement look like a fast one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FrameMetrics {
    /// Wall time from the input event that caused the frame to the frame reaching
    /// the screen. Blitz does not stamp input events with an arrival time and the
    /// renderer reports no present timestamp, so there are no two clocks to
    /// subtract.
    pub input_to_present_ms: Option<f64>,
    /// Style recalculation alone. Blitz runs style and layout inside a single
    /// `resolve` pass and times only the pass, so the combined cost is in
    /// `resolve_ms` instead.
    pub style_ms: Option<f64>,
    /// Layout alone. See `style_ms`.
    pub layout_ms: Option<f64>,
    /// Style plus layout, i.e. the `resolve` pass.
    pub resolve_ms: f64,
    /// Scene building: the `paint_scene` call that turns the resolved document
    /// into renderer commands.
    pub scene_ms: f64,
    /// GPU submit alone. The renderer reports encode, submit and present as one
    /// figure, which is in `renderer_ms`.
    pub submit_ms: Option<f64>,
    /// Present alone. See `submit_ms`.
    pub present_ms: Option<f64>,
    /// Everything the renderer did outside scene building: encode, submit and
    /// present.
    pub renderer_ms: f64,
    /// `resolve_ms + scene_ms + renderer_ms`. CPU time inside `redraw`, not
    /// input-to-photon latency.
    pub total_ms: f64,
    /// How long ago the frame started, measured when this response was built. A
    /// large value means the app has been idle and these numbers are stale.
    pub age_ms: f64,
}

/// Mean, 95th percentile and worst case for one timing series, in milliseconds.
///
/// The percentile and maximum matter more than the mean: a one-second average
/// hides the single slow frame that is the only one a user notices.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimingStats {
    pub mean_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

/// Aggregate statistics over the recently presented frames.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FrameWindowMetrics {
    /// Frames presented since process start.
    pub frames_total: u64,
    /// Frames the statistics below were computed over.
    pub window_frames: u64,
    /// Style plus layout.
    pub resolve: TimingStats,
    /// Scene building.
    pub scene: TimingStats,
    /// Encode, submit and present.
    pub renderer: TimingStats,
    /// Per-frame `resolve + scene + renderer`.
    pub total: TimingStats,
    /// Gap between the starts of consecutive frames, idle gaps excluded.
    pub interval: TimingStats,
    /// Frames per second across active intervals only. Blitz idles at zero FPS by
    /// design, so counting idle time would make every measurement meaningless.
    pub active_fps: f64,
    /// Active intervals longer than 1.5 display refresh periods. Always zero when
    /// `display_refresh_hz` is unknown.
    pub missed_refreshes: u64,
    pub display_refresh_hz: Option<f64>,
}

/// What it cost to answer this diagnostics request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotCost {
    /// Time spent draining the script event loop before reading the document.
    pub poll_ms: f64,
    /// Time spent forcing a `resolve` so the reported layout is up to date.
    pub resolve_ms: f64,
    /// Total time spent building the response.
    pub total_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleEntry {
    pub sequence: u64,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeErrorEntry {
    pub sequence: u64,
    pub message: String,
    pub stack: String,
    pub revisions: RevisionSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum LifecycleEvent {
    Relaunching { replacement_instance_id: String },
    Ready { instance_id: String },
    Quitting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DebugError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ToolCallParams {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ToolCallResult {
    content: Vec<TextContent>,
    structured_content: serde_json::Value,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
struct TextContent {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

#[derive(Debug)]
pub enum DebugProtocolError {
    Json(serde_json::Error),
    Rpc(JsonRpcError),
    NonTextFrame,
    RequestIdRequired,
    UnexpectedMessage,
    UnexpectedMethod(String),
    UnexpectedTool(String),
}

impl std::fmt::Display for DebugProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "invalid debug frame: {error}"),
            Self::Rpc(error) => write!(
                formatter,
                "JSON-RPC error {}: {}",
                error.code, error.message
            ),
            Self::NonTextFrame => formatter.write_str("debug protocol requires a text frame"),
            Self::RequestIdRequired => formatter.write_str("tools/call requires a request id"),
            Self::UnexpectedMessage => formatter.write_str("unexpected JSON-RPC message"),
            Self::UnexpectedMethod(method) => write!(formatter, "unexpected method {method}"),
            Self::UnexpectedTool(tool) => write!(formatter, "unexpected tool {tool}"),
        }
    }
}

impl std::error::Error for DebugProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for DebugProtocolError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn encode_agent_request(
    id: JsonRpcId,
    request: &AgentControlRequest,
) -> Result<WireMessage, DebugProtocolError> {
    encode_tool_request(id, AGENT_CONTROL_TOOL, request)
}

pub fn decode_incoming(message: WireMessage) -> Result<IncomingRequest, DebugProtocolError> {
    let JsonRpcMessage::Request(request) = decode_rpc(message)? else {
        return Err(DebugProtocolError::UnexpectedMessage);
    };
    match request.method.as_str() {
        MCP_INITIALIZE => Ok(IncomingRequest::Initialize {
            id: request.id.ok_or(DebugProtocolError::RequestIdRequired)?,
        }),
        MCP_INITIALIZED_NOTIFICATION if request.is_notification() => {
            Ok(IncomingRequest::Initialized)
        }
        MCP_TOOLS_LIST => Ok(IncomingRequest::ToolsList {
            id: request.id.ok_or(DebugProtocolError::RequestIdRequired)?,
        }),
        MCP_TOOLS_CALL => decode_incoming_tool_call(request),
        _ => Err(DebugProtocolError::UnexpectedMethod(request.method)),
    }
}

pub fn encode_initialize_response(
    id: JsonRpcId,
    server_version: &str,
) -> Result<WireMessage, DebugProtocolError> {
    encode_rpc(JsonRpcMessage::Response(JsonRpcResponse::result(
        Some(id),
        serde_json::json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "tauri-runtime-blitz", "version": server_version}
        }),
    )))
}

/// The tool list, which is the one place a build's *capability* shows.
///
/// `include_diagnostics` is the server's `diagnostics` feature. The protocol
/// types for diagnostics exist unconditionally, so a build that cannot collect
/// them has to say so here rather than by failing to define them. Advertising
/// a tool that answers every call with an error would be worse than omitting
/// it: a client would report the app as broken instead of as a plain build.
pub fn encode_tools_list_response(
    id: JsonRpcId,
    include_diagnostics: bool,
) -> Result<WireMessage, DebugProtocolError> {
    let mut tools = vec![serde_json::json!({
        "name": AGENT_CONTROL_TOOL,
        "description": "Inspect and operate the native Blitz semantic UI tree",
        "inputSchema": schema_for::<AgentControlRequest>()?
    })];
    if include_diagnostics {
        tools.push(serde_json::json!({
            "name": DIAGNOSTICS_TOOL,
            "description": "Observe Blitz DOM, layout, errors, and renderer metrics",
            "inputSchema": schema_for::<DiagnosticsRequest>()?
        }));
    }
    encode_rpc(JsonRpcMessage::Response(JsonRpcResponse::result(
        Some(id),
        serde_json::json!({"tools": tools}),
    )))
}

fn schema_for<T: JsonSchema>() -> Result<serde_json::Value, DebugProtocolError> {
    serde_json::to_value(schemars::schema_for!(T)).map_err(DebugProtocolError::from)
}

pub fn encode_rpc_error(
    id: Option<JsonRpcId>,
    error: JsonRpcError,
) -> Result<WireMessage, DebugProtocolError> {
    encode_rpc(JsonRpcMessage::Response(JsonRpcResponse::error(id, error)))
}

pub fn decode_agent_request(
    message: WireMessage,
) -> Result<(JsonRpcId, AgentControlRequest), DebugProtocolError> {
    decode_tool_request(message, AGENT_CONTROL_TOOL)
}

pub fn encode_diagnostics_request(
    id: JsonRpcId,
    request: &DiagnosticsRequest,
) -> Result<WireMessage, DebugProtocolError> {
    encode_tool_request(id, DIAGNOSTICS_TOOL, request)
}

pub fn decode_diagnostics_request(
    message: WireMessage,
) -> Result<(JsonRpcId, DiagnosticsRequest), DebugProtocolError> {
    decode_tool_request(message, DIAGNOSTICS_TOOL)
}

pub fn encode_response(
    id: JsonRpcId,
    response: &DebugResponse,
) -> Result<WireMessage, DebugProtocolError> {
    let structured_content = serde_json::to_value(response)?;
    let result = ToolCallResult {
        content: vec![TextContent {
            content_type: "text".into(),
            // structuredContent is the typed payload consumed by the local
            // client. Repeating a full DOM snapshot as text doubled the frame
            // and could cross endpoint-libs' 16 MiB safety limit, which closed
            // an otherwise healthy socket. Keep the required human-readable
            // MCP content concise and send the snapshot exactly once.
            text: response_summary(response),
        }],
        structured_content,
        is_error: matches!(response, DebugResponse::Error(_)),
    };
    encode_rpc(JsonRpcMessage::Response(JsonRpcResponse::result(
        Some(id),
        serde_json::to_value(result)?,
    )))
}

fn response_summary(response: &DebugResponse) -> String {
    match response {
        DebugResponse::Ack => "ok".into(),
        DebugResponse::AgentSnapshot(snapshot) => {
            format!("semantic snapshot with {} nodes", snapshot.nodes.len())
        }
        DebugResponse::Snapshot(_) => "diagnostic snapshot".into(),
        DebugResponse::Metrics(_) => "renderer metrics".into(),
        DebugResponse::Idle(_) => "renderer idle".into(),
        DebugResponse::WindowComposition(_) => "native window composition".into(),
        DebugResponse::Captured(image) => {
            format!("captured {}x{} image", image.width, image.height)
        }
        DebugResponse::Error(error) => error.message.clone(),
    }
}

pub fn decode_response(
    message: WireMessage,
) -> Result<(JsonRpcId, DebugResponse), DebugProtocolError> {
    let JsonRpcMessage::Response(response) = decode_rpc(message)? else {
        return Err(DebugProtocolError::UnexpectedMessage);
    };
    let id = response.id.ok_or(DebugProtocolError::RequestIdRequired)?;
    if let Some(error) = response.error {
        return Err(DebugProtocolError::Rpc(error));
    }
    let result: ToolCallResult = serde_json::from_value(
        response
            .result
            .ok_or(DebugProtocolError::UnexpectedMessage)?,
    )?;
    Ok((id, serde_json::from_value(result.structured_content)?))
}

pub fn encode_agent_event(event: &AgentControlEvent) -> Result<WireMessage, DebugProtocolError> {
    encode_notification(AGENT_EVENT_NOTIFICATION, event)
}

pub fn decode_agent_event(message: WireMessage) -> Result<AgentControlEvent, DebugProtocolError> {
    decode_notification(message, AGENT_EVENT_NOTIFICATION)
}

pub fn encode_diagnostics_event(event: &DebugEvent) -> Result<WireMessage, DebugProtocolError> {
    encode_notification(DIAGNOSTICS_EVENT_NOTIFICATION, event)
}

pub fn decode_diagnostics_event(message: WireMessage) -> Result<DebugEvent, DebugProtocolError> {
    decode_notification(message, DIAGNOSTICS_EVENT_NOTIFICATION)
}

fn encode_tool_request<T: Serialize>(
    id: JsonRpcId,
    tool: &str,
    request: &T,
) -> Result<WireMessage, DebugProtocolError> {
    let params = ToolCallParams {
        name: tool.into(),
        arguments: serde_json::to_value(request)?,
    };
    encode_rpc(JsonRpcMessage::Request(JsonRpcRequest::call(
        id,
        MCP_TOOLS_CALL,
        serde_json::to_value(params)?,
    )))
}

fn decode_tool_request<T: DeserializeOwned>(
    message: WireMessage,
    expected_tool: &str,
) -> Result<(JsonRpcId, T), DebugProtocolError> {
    let JsonRpcMessage::Request(request) = decode_rpc(message)? else {
        return Err(DebugProtocolError::UnexpectedMessage);
    };
    if request.method != MCP_TOOLS_CALL {
        return Err(DebugProtocolError::UnexpectedMethod(request.method));
    }
    let id = request.id.ok_or(DebugProtocolError::RequestIdRequired)?;
    let params: ToolCallParams = serde_json::from_value(request.params.ok_or_else(|| {
        DebugProtocolError::Rpc(JsonRpcError::new(
            INVALID_PARAMS,
            "missing tools/call params",
        ))
    })?)?;
    if params.name != expected_tool {
        return Err(DebugProtocolError::UnexpectedTool(params.name));
    }
    Ok((id, serde_json::from_value(params.arguments)?))
}

fn decode_incoming_tool_call(
    request: JsonRpcRequest,
) -> Result<IncomingRequest, DebugProtocolError> {
    let id = request.id.ok_or(DebugProtocolError::RequestIdRequired)?;
    let params: ToolCallParams = serde_json::from_value(request.params.ok_or_else(|| {
        DebugProtocolError::Rpc(JsonRpcError::new(
            INVALID_PARAMS,
            "missing tools/call params",
        ))
    })?)?;
    match params.name.as_str() {
        AGENT_CONTROL_TOOL => Ok(IncomingRequest::Agent {
            id,
            request: serde_json::from_value(params.arguments)?,
        }),
        DIAGNOSTICS_TOOL => Ok(IncomingRequest::Diagnostics {
            id,
            request: serde_json::from_value(params.arguments)?,
        }),
        _ => Err(DebugProtocolError::UnexpectedTool(params.name)),
    }
}

fn encode_notification<T: Serialize>(
    method: &str,
    value: &T,
) -> Result<WireMessage, DebugProtocolError> {
    encode_rpc(JsonRpcMessage::Request(JsonRpcRequest::notification(
        method,
        serde_json::to_value(value)?,
    )))
}

fn decode_notification<T: DeserializeOwned>(
    message: WireMessage,
    expected_method: &str,
) -> Result<T, DebugProtocolError> {
    let JsonRpcMessage::Request(request) = decode_rpc(message)? else {
        return Err(DebugProtocolError::UnexpectedMessage);
    };
    if !request.is_notification() {
        return Err(DebugProtocolError::Rpc(JsonRpcError::new(
            INVALID_REQUEST,
            "notification must not contain an id",
        )));
    }
    if request.method != expected_method {
        return Err(DebugProtocolError::UnexpectedMethod(request.method));
    }
    serde_json::from_value(request.params.ok_or_else(|| {
        DebugProtocolError::Rpc(JsonRpcError::new(
            INVALID_PARAMS,
            "missing notification params",
        ))
    })?)
    .map_err(Into::into)
}

pub fn encode_rpc(message: JsonRpcMessage) -> Result<WireMessage, DebugProtocolError> {
    Ok(WireMessage::Text(serde_json::to_string(&message)?))
}

/// The request id of an incoming frame, recovered without consuming it.
///
/// A decode failure still has to be answered, and answered *to the caller*. A
/// JSON-RPC client matches responses by id, so an error carrying `id: null` is
/// delivered and then discarded, and the caller waits until it times out. That
/// turns "you named a field wrong" into "the app is hung", which is a much
/// worse and much slower thing to debug.
pub fn peek_request_id(message: &WireMessage) -> Option<JsonRpcId> {
    let WireMessage::Text(text) = message else {
        return None;
    };
    match parse(text) {
        Ok(JsonRpcMessage::Request(request)) => request.id,
        _ => None,
    }
}

pub fn decode_rpc(message: WireMessage) -> Result<JsonRpcMessage, DebugProtocolError> {
    let WireMessage::Text(text) = message else {
        return Err(DebugProtocolError::NonTextFrame);
    };
    parse(&text).map_err(DebugProtocolError::Rpc)
}

#[cfg(test)]
mod tests {
    use endpoint_libs::libs::ws::MessageStream;
    use endpoint_libs::libs::ws::transport::{TransportStream, framed_json};

    use super::*;

    fn key_request() -> AgentControlRequest {
        AgentControlRequest::Act(AgentAction::Input(InputCommand::Key {
            phase: KeyPhase::Down,
            key: "2".into(),
            code: "Digit2".into(),
            modifiers: Modifiers {
                meta: true,
                ..Default::default()
            },
        }))
    }

    #[test]
    fn typed_modifier_input_round_trips_mcp_tools_call() {
        let request = key_request();
        let id = JsonRpcId::Number(7);
        assert_eq!(
            decode_agent_request(encode_agent_request(id.clone(), &request).unwrap()).unwrap(),
            (id, request)
        );
    }

    #[test]
    fn node_addressed_actions_round_trip() {
        for action in [
            AgentAction::Click { node_id: 9 },
            AgentAction::DoubleClick { node_id: 10 },
            AgentAction::Hover { node_id: 11 },
        ] {
            let request = AgentControlRequest::Act(action);
            let id = JsonRpcId::Number(8);
            assert_eq!(
                decode_agent_request(encode_agent_request(id.clone(), &request).unwrap()).unwrap(),
                (id, request)
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn endpoint_framing_carries_debug_frames_without_a_session_or_token() {
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let mut server = TransportStream::new(framed_json(server_io));
        let mut client = TransportStream::new(framed_json(client_io));
        let request = key_request();
        let id = JsonRpcId::String("input-7".into());

        client
            .send(encode_agent_request(id.clone(), &request).unwrap())
            .await
            .unwrap();
        let received = server.recv().await.unwrap().unwrap();

        assert_eq!(decode_agent_request(received).unwrap(), (id, request));
    }

    #[test]
    fn unrelated_mcp_tool_is_rejected_explicitly() {
        let message = JsonRpcMessage::Request(JsonRpcRequest::call(
            JsonRpcId::Number(1),
            MCP_TOOLS_CALL,
            serde_json::json!({"name": "other.tool", "arguments": {}}),
        ));
        assert!(matches!(
            decode_agent_request(encode_rpc(message).unwrap()),
            Err(DebugProtocolError::UnexpectedTool(tool)) if tool == "other.tool"
        ));
    }

    #[test]
    fn responses_and_notifications_round_trip() {
        let id = JsonRpcId::Number(19);
        let response = DebugResponse::Ack;
        assert_eq!(
            decode_response(encode_response(id.clone(), &response).unwrap()).unwrap(),
            (id, response)
        );

        let event = AgentControlEvent::TreeChanged { revision: 12 };
        assert_eq!(
            decode_agent_event(encode_agent_event(&event).unwrap()).unwrap(),
            event
        );

        let event = DebugEvent::PaintCommitted { revision: 13 };
        assert_eq!(
            decode_diagnostics_event(encode_diagnostics_event(&event).unwrap()).unwrap(),
            event
        );
    }

    #[test]
    fn diagnostics_metrics_request_and_response_round_trip() {
        let id = JsonRpcId::Number(23);
        assert_eq!(
            decode_diagnostics_request(
                encode_diagnostics_request(id.clone(), &DiagnosticsRequest::Metrics).unwrap()
            )
            .unwrap(),
            (id.clone(), DiagnosticsRequest::Metrics)
        );

        let response = DebugResponse::Metrics(RendererMetrics {
            resident_bytes: Some(4096),
            ..Default::default()
        });
        assert_eq!(
            decode_response(encode_response(id.clone(), &response).unwrap()).unwrap(),
            (id, response)
        );
    }

    #[test]
    fn native_window_composition_round_trips_without_pixel_inference() {
        let id = JsonRpcId::Number(24);
        assert_eq!(
            decode_diagnostics_request(
                encode_diagnostics_request(id.clone(), &DiagnosticsRequest::WindowComposition)
                    .unwrap()
            )
            .unwrap(),
            (id.clone(), DiagnosticsRequest::WindowComposition)
        );

        let response = DebugResponse::WindowComposition(WindowComposition {
            supported: true,
            surface_transparent: true,
            glass_enabled: true,
            glass_backend: Some("nativeGlass".into()),
            tint_rgba: Some([174, 50, 112, 0]),
            radius: Some(12.0),
        });
        assert_eq!(
            decode_response(encode_response(id.clone(), &response).unwrap()).unwrap(),
            (id, response)
        );
    }

    #[test]
    fn tools_list_describes_the_typed_requests() {
        let message = encode_tools_list_response(JsonRpcId::Number(31), true).unwrap();
        let JsonRpcMessage::Response(response) = decode_rpc(message).unwrap() else {
            panic!("tools/list should be a response")
        };
        let tools = response.result.unwrap()["tools"]
            .as_array()
            .unwrap()
            .to_vec();

        let agent = &tools[0]["inputSchema"];
        let diagnostics = &tools[1]["inputSchema"];
        assert!(
            agent.get("oneOf").is_some() || agent.get("$defs").is_some(),
            "agent schema must describe its tagged command variants: {agent}"
        );
        assert!(
            diagnostics.get("oneOf").is_some() || diagnostics.get("$defs").is_some(),
            "diagnostics schema must describe its tagged command variants: {diagnostics}"
        );
        assert_ne!(agent, &serde_json::json!({"type": "object"}));
        assert_ne!(diagnostics, &serde_json::json!({"type": "object"}));
        let agent_text = agent.to_string();
        let diagnostics_text = diagnostics.to_string();
        assert!(agent_text.contains("command") && agent_text.contains("inspect"));
        assert!(diagnostics_text.contains("command") && diagnostics_text.contains("metrics"));
    }

    #[test]
    fn typed_layout_row_preserves_the_existing_wire_shape() {
        let row = LayoutDiagnosticRow {
            node_id: 7,
            bounds: LayoutBounds {
                x: 1.0,
                y: 2.0,
                width: 30.0,
                height: 40.0,
            },
            scroll_offset: LayoutOffset { x: 3.0, y: 4.0 },
            client_size: LayoutSize {
                width: 30.0,
                height: 40.0,
            },
            scroll_size: LayoutSize {
                width: 50.0,
                height: 60.0,
            },
            scroll_range: LayoutSize {
                width: 20.0,
                height: 20.0,
            },
            border: LayoutEdges {
                top: 1.0,
                right: 2.0,
                bottom: 3.0,
                left: 4.0,
            },
            padding: LayoutEdges {
                top: 5.0,
                right: 6.0,
                bottom: 7.0,
                left: 8.0,
            },
            content_size: LayoutSize {
                width: 10.0,
                height: 11.0,
            },
        };
        let old_wire_shape = serde_json::json!({
            "nodeId": 7,
            "bounds": [1.0, 2.0, 30.0, 40.0],
            "scrollOffset": [3.0, 4.0],
            "clientSize": [30.0, 40.0],
            "scrollSize": [50.0, 60.0],
            "scrollRange": [20.0, 20.0],
            "border": [1.0, 2.0, 3.0, 4.0],
            "padding": [5.0, 6.0, 7.0, 8.0],
            "contentSize": [10.0, 11.0]
        });

        assert_eq!(serde_json::to_value(&row).unwrap(), old_wire_shape);
        assert_eq!(
            serde_json::from_value::<LayoutDiagnosticRow>(old_wire_shape).unwrap(),
            row
        );
    }

    #[test]
    fn semantic_node_keeps_authored_dom_identity_separate_from_node_handle() {
        let node = SemanticNode {
            dom_id: Some("settings-save".into()),
            id: 42,
            parent: None,
            role: "button".into(),
            name: "Save settings".into(),
            value: None,
            enabled: true,
            visible: true,
            selected: false,
            bounds: Some([1.0, 2.0, 30.0, 20.0]),
            slot: Some("button".into()),
        };

        let encoded = serde_json::to_value(&node).unwrap();
        assert_eq!(encoded["domId"], "settings-save");
        assert_eq!(encoded["id"], 42);
        assert_eq!(
            serde_json::from_value::<SemanticNode>(encoded).unwrap(),
            node
        );
    }

    #[test]
    fn frame_metrics_round_trip_and_keep_unmeasured_fields_empty() {
        let id = JsonRpcId::Number(24);
        let response = DebugResponse::Metrics(RendererMetrics {
            frame: Some(FrameMetrics {
                resolve_ms: 1.5,
                scene_ms: 2.25,
                renderer_ms: 3.0,
                total_ms: 6.75,
                age_ms: 12.0,
                ..Default::default()
            }),
            frame_window: Some(FrameWindowMetrics {
                frames_total: 900,
                window_frames: 256,
                total: TimingStats {
                    mean_ms: 6.0,
                    p95_ms: 11.0,
                    max_ms: 40.0,
                },
                active_fps: 59.4,
                missed_refreshes: 2,
                display_refresh_hz: Some(60.0),
                ..Default::default()
            }),
            snapshot: Some(SnapshotCost {
                poll_ms: 0.5,
                resolve_ms: 1.0,
                total_ms: 4.0,
            }),
            ..Default::default()
        });

        let (decoded_id, decoded) =
            decode_response(encode_response(id.clone(), &response).unwrap()).unwrap();
        assert_eq!(decoded_id, id);
        assert_eq!(decoded, response);

        let DebugResponse::Metrics(metrics) = decoded else {
            panic!("expected renderer metrics")
        };
        let frame = metrics.frame.unwrap();
        // Fields blitz does not measure must stay empty on the wire. Sending 0.0
        // is the defect this guards: a zero reads as "measured, and fast".
        assert_eq!(frame.input_to_present_ms, None);
        assert_eq!(frame.style_ms, None);
        assert_eq!(frame.layout_ms, None);
        assert_eq!(frame.submit_ms, None);
        assert_eq!(frame.present_ms, None);
        assert_eq!(metrics.queue_depth, None);
    }
}
