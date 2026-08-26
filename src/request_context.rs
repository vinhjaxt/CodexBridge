use axum::http::request::Parts;
use rmcp::{
    RoleServer,
    handler::server::{common::FromContextPart, tool::ToolCallContext},
    model::RequestMetaObject,
    service::RequestContext,
};
use serde::Serialize;
use serde_json::Value;

use crate::{error::AppError, project::ProjectContext};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportMode {
    Stateless,
    LegacySession,
}

#[derive(Debug, Clone)]
pub struct RequestIdentity {
    pub openai_subject: String,
    pub openai_conversation_id: String,
    pub mcp_session_id: Option<String>,
    pub transport_mode: TransportMode,
}

#[derive(Debug)]
pub struct ProjectRequestContext(pub crate::error::Result<ProjectContext>);

#[derive(Debug)]
pub struct InitializationRequestContext(pub crate::error::Result<RequestIdentity>);

pub fn identity_from_request(
    context: &RequestContext<RoleServer>,
) -> crate::error::Result<RequestIdentity> {
    let mcp_session_id = context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.headers.get("mcp-session-id"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    identity_from_meta(&context.meta, mcp_session_id)
}

fn identity_from_meta(
    meta: &RequestMetaObject,
    mcp_session_id: Option<String>,
) -> crate::error::Result<RequestIdentity> {
    let get = |key: &str| {
        meta.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let openai_subject = get("openai/subject").ok_or_else(|| {
        AppError::new(
            "INCOMPLETE_OPENAI_CONTEXT",
            "MISSING_OPENAI_SUBJECT: request metadata must contain openai/subject",
        )
    })?;
    let openai_conversation_id = get("openai/session").ok_or_else(|| {
        AppError::new(
            "INCOMPLETE_OPENAI_CONTEXT",
            "MISSING_OPENAI_SESSION: request metadata must contain openai/session",
        )
    })?;
    let transport_mode = if mcp_session_id.is_some() {
        TransportMode::LegacySession
    } else {
        TransportMode::Stateless
    };
    Ok(RequestIdentity {
        openai_subject,
        openai_conversation_id,
        mcp_session_id,
        transport_mode,
    })
}

impl<S> FromContextPart<ToolCallContext<'_, S>> for InitializationRequestContext {
    fn from_context_part(context: &mut ToolCallContext<'_, S>) -> Result<Self, rmcp::ErrorData> {
        // Keep request-context failures inside the tool handler instead of
        // escaping MCP ErrorData from this extractor. The handler can then
        // distinguish hard execution failures from recoverable continuity
        // failures that intentionally return an MCP-success soft stop.
        Ok(Self(identity_from_request(&context.request_context)))
    }
}

impl FromContextPart<ToolCallContext<'_, crate::tools::AgentHandler>> for ProjectRequestContext {
    fn from_context_part(
        context: &mut ToolCallContext<'_, crate::tools::AgentHandler>,
    ) -> Result<Self, rmcp::ErrorData> {
        let project = identity_from_request(&context.request_context).and_then(|identity| {
            context
                .service
                .shared
                .resolver
                .resolve_initialized(&identity)
        });
        // As with chatgpt_turn_init, project-context failures are execution
        // failures for a tool call, not JSON-RPC protocol failures. Carry the
        // error into the handler so it can return CallToolResult::isError and a
        // strict MCP client does not tear down its task group.
        Ok(Self(project))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_openai_metadata_is_rejected() {
        for (key, value) in [("openai/subject", "usr"), ("openai/session", "conv")] {
            let mut meta = serde_json::Map::new();
            meta.insert(key.to_owned(), Value::String(value.to_owned()));
            let error = identity_from_meta(&RequestMetaObject(rmcp::model::MetaObject(meta)), None)
                .unwrap_err();
            assert_eq!(error.code(), "INCOMPLETE_OPENAI_CONTEXT");
        }
    }

    #[test]
    fn empty_or_non_string_openai_metadata_is_rejected() {
        for invalid in [Value::String(String::new()), Value::Null, Value::Bool(true)] {
            let mut meta = serde_json::Map::new();
            meta.insert("openai/subject".to_owned(), invalid.clone());
            meta.insert(
                "openai/session".to_owned(),
                Value::String("conversation".to_owned()),
            );
            assert_eq!(
                identity_from_meta(&RequestMetaObject(rmcp::model::MetaObject(meta)), None)
                    .unwrap_err()
                    .code(),
                "INCOMPLETE_OPENAI_CONTEXT"
            );

            let mut meta = serde_json::Map::new();
            meta.insert(
                "openai/subject".to_owned(),
                Value::String("subject".to_owned()),
            );
            meta.insert("openai/session".to_owned(), invalid);
            assert_eq!(
                identity_from_meta(&RequestMetaObject(rmcp::model::MetaObject(meta)), None)
                    .unwrap_err()
                    .code(),
                "INCOMPLETE_OPENAI_CONTEXT"
            );
        }
    }

    #[test]
    fn complete_metadata_preserves_identity_and_transport_mode() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "openai/subject".to_owned(),
            Value::String("subject".to_owned()),
        );
        meta.insert(
            "openai/session".to_owned(),
            Value::String("conversation".to_owned()),
        );
        let stateless = identity_from_meta(
            &RequestMetaObject(rmcp::model::MetaObject(meta.clone())),
            None,
        )
        .unwrap();
        assert_eq!(stateless.openai_subject, "subject");
        assert_eq!(stateless.openai_conversation_id, "conversation");
        assert_eq!(stateless.transport_mode, TransportMode::Stateless);
        assert_eq!(stateless.mcp_session_id, None);

        let legacy = identity_from_meta(
            &RequestMetaObject(rmcp::model::MetaObject(meta)),
            Some("transport-session".to_owned()),
        )
        .unwrap();
        assert_eq!(legacy.openai_subject, "subject");
        assert_eq!(legacy.openai_conversation_id, "conversation");
        assert_eq!(legacy.transport_mode, TransportMode::LegacySession);
        assert_eq!(legacy.mcp_session_id.as_deref(), Some("transport-session"));
    }
    #[test]
    fn identity_distinguishes_subject_and_conversation_axes() {
        // Same subject + different conversation => different native identity;
        // same conversation id under a different subject also differs. The
        // native key is a derived opaque token, never the raw metadata.
        fn native(subject: &str, conversation: &str) -> crate::project::ProjectKey {
            let mut meta = serde_json::Map::new();
            meta.insert(
                "openai/subject".to_owned(),
                Value::String(subject.to_owned()),
            );
            meta.insert(
                "openai/session".to_owned(),
                Value::String(conversation.to_owned()),
            );
            let identity =
                identity_from_meta(&RequestMetaObject(rmcp::model::MetaObject(meta)), None)
                    .unwrap();
            crate::project::derive_native_project_key(
                &identity.openai_subject,
                &identity.openai_conversation_id,
            )
        }

        let base = native("usr_a", "conv_1");
        assert_ne!(base, native("usr_a", "conv_2"));
        assert_ne!(base, native("usr_b", "conv_1"));
        assert_eq!(base, native("usr_a", "conv_1"), "identity must be stable");
        assert!(!base.as_str().contains("usr_a"));
        assert!(!base.as_str().contains("conv_1"));
    }
}
