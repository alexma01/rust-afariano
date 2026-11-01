//! `.gitlab-ci.yml` subset → pipeline plan. Pure: no I/O. See `docs/DESIGN.md` § CI subset.

use std::collections::BTreeMap;

mod duration;
mod expr;
mod extends;
mod plan;
mod yaml;

pub use plan::evaluate_if;

pub use rust_afariano_proto::{ImageSpec, ServiceSpec};
use serde::{Deserialize, Serialize};

/// What the caller knows before reading the YAML.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Predefined variables (`CI_PIPELINE_SOURCE`, `CI_COMMIT_BRANCH`,
    /// `CI_COMMIT_TAG`, `CI_COMMIT_REF_NAME`, `CI_DEFAULT_BRANCH`, …) plus project
    /// and pipeline variables. They win over YAML variables in `rules:if`.
    pub variables: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// `workflow:rules` (or `when: never`) say no pipeline for this context.
    NoPipeline {
        reason: String,
    },
    /// Workflow allows it but no job survived its rules.
    Empty,
    Pipeline(Plan),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// In order, including `.pre` first and `.post` last.
    pub stages: Vec<String>,
    /// Global YAML `variables` plus `workflow:rules:variables` (the latter win).
    pub variables: Vec<(String, String)>,
    /// Ordered by stage index, then by order of appearance in the file.
    pub jobs: Vec<Job>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum When {
    OnSuccess,
    OnFailure,
    Always,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    pub name: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub name: String,
    pub stage: String,
    /// Index into [`Plan::stages`].
    pub stage_idx: usize,
    pub when: When,
    pub allow_failure: bool,
    /// `Some` when the job declares `needs` (then stages do not order it).
    /// Optional needs that point at absent jobs are dropped.
    pub needs: Option<Vec<String>>,
    pub image: Option<ImageSpec>,
    pub services: Vec<ServiceSpec>,
    pub before_script: Vec<String>,
    pub script: Vec<String>,
    pub after_script: Vec<String>,
    /// Job YAML variables, then `rules:variables` of the matching rule (those win).
    /// Global variables are in [`Plan::variables`], not repeated here.
    pub variables: Vec<(String, String)>,
    pub tags: Vec<String>,
    pub environment: Option<Environment>,
    pub resource_group: Option<String>,
    pub timeout_secs: Option<u64>,
    pub interruptible: bool,
    pub retry: u32,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Error {
    #[error("invalid YAML: {0}")]
    Yaml(String),
    /// A semantic problem, phrased for the pipeline page ("job `x`: unknown stage `y`").
    #[error("{0}")]
    Invalid(String),
}

/// Parse `yaml` and evaluate workflow and job rules against `ctx`.
pub fn plan(yaml: &str, ctx: &Context) -> Result<Outcome, Error> {
    plan::plan(yaml, ctx)
}

/// GitLab duration (`30m`, `1h 30m`, `90 minutes`, `2 hours`, `3600`) → seconds.
pub fn parse_duration(s: &str) -> Option<u64> {
    duration::parse(s)
}
