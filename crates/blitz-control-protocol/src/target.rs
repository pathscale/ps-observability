use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AgentAction, AgentControlRequest, DebugError, DebugEvent, DebugResponse, DiagnosticsRequest,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TargetId {
    Chrome(Chrome),
    Tab(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Chrome {
    #[serde(rename = "chrome")]
    Chrome,
}

impl TargetId {
    pub fn chrome() -> Self {
        Self::Chrome(Chrome::Chrome)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TargetSelector {
    Chrome(Chrome),
    Tab {
        #[serde(rename = "tabId")]
        tab_id: u64,
    },
}

impl TargetSelector {
    pub fn id(&self) -> TargetId {
        match self {
            Self::Chrome(_) => TargetId::chrome(),
            Self::Tab { tab_id } => TargetId::Tab(*tab_id),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_document_id: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_targets: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sequence: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TargetEnvelope<T> {
    #[serde(flatten)]
    pub options: TargetOptions,
    #[serde(flatten)]
    pub request: T,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TargetCommand {
    Agent(AgentControlRequest),
    Diagnostics(DiagnosticsRequest),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetRequest {
    pub options: TargetOptions,
    pub command: TargetCommand,
}

impl TargetRequest {
    pub fn uses_nodes(&self) -> bool {
        match &self.command {
            TargetCommand::Agent(AgentControlRequest::Inspect { root, .. }) => root.is_some(),
            TargetCommand::Agent(AgentControlRequest::Act(action)) => {
                !matches!(action, AgentAction::Input(_))
            }
            TargetCommand::Diagnostics(DiagnosticsRequest::Snapshot(request)) => {
                !request.node_ids.is_empty()
            }
            TargetCommand::Diagnostics(DiagnosticsRequest::Capture(request)) => {
                request.node_id.is_some()
            }
            _ => false,
        }
    }

    pub fn instance_scoped(&self) -> bool {
        matches!(
            &self.command,
            TargetCommand::Agent(
                AgentControlRequest::ListTargets
                    | AgentControlRequest::Quit
                    | AgentControlRequest::Relaunch
            ) | TargetCommand::Diagnostics(DiagnosticsRequest::WindowComposition)
        )
    }

    pub fn validate_options(&self) -> Result<(), DebugError> {
        let observing = matches!(
            &self.command,
            TargetCommand::Diagnostics(DiagnosticsRequest::Observe { .. })
        );
        if (self.options.all_targets || self.options.after_sequence.is_some()) && !observing {
            return Err(error(
                "invalidArgument",
                "allTargets and afterSequence are Observe options",
            ));
        }
        if self.options.all_targets && self.options.target.is_some() {
            return Err(error(
                "invalidArgument",
                "allTargets and target are mutually exclusive",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetContext {
    pub target_id: TargetId,
    pub generation: u64,
    pub document_id: u64,
}

impl TargetContext {
    pub fn validate(&self, request: &TargetRequest) -> Result<(), DebugError> {
        if matches!(&self.target_id, TargetId::Tab(_))
            && request.uses_nodes()
            && (request.options.expected_generation.is_none()
                || request.options.expected_document_id.is_none())
        {
            return Err(error(
                "targetIdentityRequired",
                "tab node IDs require expectedGeneration and expectedDocumentId",
            ));
        }
        if request
            .options
            .expected_generation
            .is_some_and(|value| value != self.generation)
        {
            return Err(error(
                "staleGeneration",
                "the mounted target generation changed",
            ));
        }
        if request
            .options
            .expected_document_id
            .is_some_and(|value| value != self.document_id)
        {
            return Err(error("staleDocument", "the mounted document changed"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetCapabilities {
    pub agent: bool,
    pub diagnostics: bool,
    pub capture: bool,
    pub console: bool,
    pub runtime_errors: bool,
    pub web_gpu: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    pub id: TargetId,
    pub url: String,
    pub title: String,
    pub active: bool,
    pub generation: u64,
    pub document_id: Option<u64>,
    pub capabilities: TargetCapabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TargetReply {
    pub context: TargetContext,
    #[serde(flatten)]
    pub response: Box<DebugResponse>,
}

impl DebugResponse {
    pub fn with_context(self, context: TargetContext) -> Self {
        if matches!(&self, Self::Error(_)) {
            self
        } else {
            Self::Targeted(TargetReply {
                context,
                response: Box::new(self),
            })
        }
    }

    pub fn unscoped(&self) -> &Self {
        match self {
            Self::Targeted(reply) => reply.response.unscoped(),
            other => other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticRecord {
    #[serde(flatten)]
    pub context: TargetContext,
    pub sequence: u64,
    #[serde(flatten)]
    pub event: DebugEvent,
}

pub fn error(code: &str, message: &str) -> DebugError {
    DebugError {
        code: code.into(),
        message: message.into(),
    }
}
