//! Shared context constants and encoding helpers.
//! Schema version strings stay here as `&str` and must not be normalized.

pub(super) const VECTOR_DIM: usize = 128;
pub(super) const STYLE_USER: &str = "최연우";
pub const STYLE_POLICY_VERSION: &str = "ordinary-conversation-v3";
pub const CONTEXT_REPLY_BUNDLE_MAX_EXCLUDED_LOG_IDS: usize = 6;
pub(super) const RETRIEVAL_INDEX_SCHEMA_VERSION: &str = "2";
pub(super) const CONTEXT_RETRIEVAL_MIGRATION_REQUIRED: &str =
    "context retrieval index migration required; run context-index";
