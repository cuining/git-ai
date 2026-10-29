//! Token-usage extraction from agent transcripts.
//!
//! Computes per-session token usage and estimated cost in 5-minute UTC
//! buckets. The parsing and deduplication logic is ported from ccusage
//! (<https://github.com/ccusage/ccusage>, MIT License, Copyright (c) 2025
//! @ryoppippi) and adapted to git-ai's incremental transcript streaming:
//! whole-file passes become line-oriented extractors whose state persists
//! between runs, and deduplication moves into a database so entries can
//! stream in across runs. Per-extractor deviations are documented in
//! `claude.rs` and `codex.rs`.

pub mod claude;
pub mod codex;
pub mod cost;
pub mod db;
pub mod extractor;
pub(crate) mod native_store;
pub(crate) mod opencode;
pub mod types;

pub use extractor::{UsageExtractor, extractor_for_tool};
pub use types::{PricingShape, Speed, TokenCounts, UsageEntry, bucket_ts};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageSource {
    Jsonl,
    OpenCodeSqlite,
}

pub(crate) fn usage_source(tool: &str, format: &str) -> Option<UsageSource> {
    use crate::streams::sweep::StreamFormat;
    let format = match format {
        "ClaudeJsonl" => StreamFormat::ClaudeJsonl,
        "CodexJsonl" => StreamFormat::CodexJsonl,
        "OpenCodeSqlite" => StreamFormat::OpenCodeSqlite,
        _ => return None,
    };
    match (tool, format) {
        ("claude", StreamFormat::ClaudeJsonl) | ("codex", StreamFormat::CodexJsonl) => {
            Some(UsageSource::Jsonl)
        }
        ("opencode", StreamFormat::OpenCodeSqlite)
            if crate::config::Config::get()
                .feature_flags()
                .enterprise_metrics =>
        {
            Some(UsageSource::OpenCodeSqlite)
        }
        _ => None,
    }
}
