use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Page-owned WebGPU state. Renderer and compositor devices are excluded.
///
/// Resources are live native objects. Errors are drained by inspection, while
/// their totals persist until the page is replaced. Frames are oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebGpuReport {
    pub devices: Vec<WebGpuDevice>,
    pub resources: Vec<WebGpuResource>,
    pub errors: WebGpuErrors,
    pub frames: Vec<WebGpuFrame>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebGpuDevice {
    pub handle: u64,
    pub adapter_name: String,
    pub backend: String,
    pub label: String,
    pub enabled_features: Vec<String>,
    /// Enabled limits whose values differ from WebGPU defaults.
    pub limits: BTreeMap<String, u64>,
    pub lost: bool,
    pub lost_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebGpuResource {
    pub device: u64,
    pub handle: u64,
    pub label: String,
    pub details: WebGpuResourceDetails,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum WebGpuResourceDetails {
    Buffer {
        size: u64,
        usage: u32,
    },
    Texture {
        size: [u32; 3],
        format: String,
        mip_levels: u32,
        sample_count: u32,
        usage: u32,
    },
    Sampler,
    ShaderModule {
        entry_points: Vec<WebGpuEntryPoint>,
        compilation_messages: Vec<WebGpuCompilationMessage>,
    },
    RenderPipeline {
        vertex_entry_point: Option<String>,
        fragment_entry_point: Option<String>,
        color_targets: Vec<Option<String>>,
    },
    ComputePipeline {
        compute_entry_point: Option<String>,
    },
    BindGroup,
    BindGroupLayout,
    PipelineLayout,
}

impl WebGpuResourceDetails {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Buffer { .. } => "buffer",
            Self::Texture { .. } => "texture",
            Self::Sampler => "sampler",
            Self::ShaderModule { .. } => "shader-module",
            Self::RenderPipeline { .. } => "render-pipeline",
            Self::ComputePipeline { .. } => "compute-pipeline",
            Self::BindGroup => "bind-group",
            Self::BindGroupLayout => "bind-group-layout",
            Self::PipelineLayout => "pipeline-layout",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebGpuEntryPoint {
    pub stage: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebGpuCompilationMessage {
    pub message: String,
    #[serde(rename = "type")]
    pub message_type: String,
    pub line_num: u32,
    pub line_pos: usize,
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebGpuErrors {
    pub since_previous: Vec<WebGpuError>,
    pub total: u64,
    pub validation_total: u64,
    pub uncaptured_total: u64,
    /// Entries omitted from this interval by the bounded error journal.
    /// Totals include those entries.
    pub dropped_since_previous: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebGpuError {
    pub device: u64,
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
    pub api_call: String,
    pub object_label: String,
    pub uncaptured: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebGpuFrame {
    pub sequence: u64,
    pub device: u64,
    pub submits: u64,
    /// False when encoding was outside the inspection lease, a referenced
    /// bundle was not sampled, or the bounded pass report was truncated.
    pub complete: bool,
    pub passes: Vec<WebGpuPass>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebGpuPass {
    pub kind: String,
    pub label: String,
    pub color_targets: Vec<Option<WebGpuColorTarget>>,
    pub depth_target: Option<WebGpuDepthTarget>,
    pub draws: u64,
    pub draw_indexed: u64,
    pub draw_indirect: u64,
    pub draw_indexed_indirect: u64,
    /// Direct and indirect dispatch calls.
    pub dispatches: u64,
    /// Direct vertex/index counts multiplied by their instance counts.
    /// Indirect arguments are not read back.
    pub vertices: u64,
    /// Instance counts of direct draw calls. Indirect arguments are excluded.
    pub instances: u64,
    pub pipelines: Vec<String>,
    pub complete: bool,
}

impl WebGpuPass {
    pub fn draw_count(&self) -> u64 {
        self.draws
            .saturating_add(self.draw_indexed)
            .saturating_add(self.draw_indirect)
            .saturating_add(self.draw_indexed_indirect)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebGpuColorTarget {
    /// Texture label, canvas:#dom-id, or canvas:@native-handle.
    pub target: String,
    pub resolve_target: Option<String>,
    pub load_op: String,
    pub store_op: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebGpuDepthTarget {
    pub target: String,
    pub depth_load_op: Option<String>,
    pub depth_store_op: Option<String>,
    pub stencil_load_op: Option<String>,
    pub stencil_store_op: Option<String>,
}
