//! Fix engine for applying pattern-based and LLM-assisted code migration fixes.
//!
//! This crate provides the core fix planning and application logic:
//! - `context`: trait for framework-specific LLM prompt customization
//! - `registry`: registry of FixContext implementations
//! - `engine`: pattern-based fix planning/applying (rename, prop removal, import path change, etc.)
//! - `llm_client`: OpenAI-compatible LLM client for AI-assisted fixes
//! - `goose_client`: goose CLI subprocess client for AI-assisted fixes

pub mod context;
pub mod engine;
pub mod goose_client;
pub mod llm_client;
pub mod registry;
