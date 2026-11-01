//! Static validation of the whole file, then workflow and job rules against a context.

use std::collections::{BTreeMap, HashMap, HashSet};

use rust_afariano_proto::{ImageSpec, ServiceSpec, Variable};

use crate::extends::Resolver;
use crate::yaml::{self, Map, Value};
use crate::{Context, Environment, Error, Job, Outcome, Plan, When, duration, expr};

/// Top-level keys that are not jobs.
const TOP_LEVEL_KEYWORDS: &[&str] = &[
    "default",
    "include",
    "stages",
    "types",
    "variables",
    "workflow",
    "image",
    "services",
    "cache",
    "before_script",
    "after_script",
    "spec",
];

const JOB_KEYS: &[&str] = &[
    "stage",
    "image",
    "services",
    "before_script",
    "script",
    "after_script",
    "variables",
    "needs",
    "rules",
    "only",
    "except",
    "when",
    "allow_failure",
    "environment",
    "resource_group",
    "timeout",
    "tags",
    "interruptible",
    "retry",
    "dependencies",
];

/// Harmless here: accepted and ignored.
const IGNORED_JOB_KEYS: &[&str] = &["coverage", "manual_confirmation"];

/// GitLab keys whose behaviour rust-afariano does not implement; silently ignoring
/// them would make a pipeline do something other than what it says.
const UNSUPPORTED_JOB_KEYS: &[&str] = &[
    "trigger",
    "parallel",
    "artifacts",
    "cache",
    "pages",
    "publish",
    "release",
    "secrets",
    "inherit",
    "hooks",
    "id_tokens",
    "identity",
    "start_in",
    "dast_configuration",
    "run",
    "spec",
    "include",
];

const DEFAULT_KEYS: &[&str] =
    &["image", "services", "before_script", "after_script", "tags", "timeout", "interruptible", "retry"];
const UNSUPPORTED_DEFAULT_KEYS: &[&str] = &["artifacts", "cache", "hooks", "id_tokens", "identity"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhenRaw {
    OnSuccess,
    OnFailure,
    Always,
    Manual,
    Never,
}

impl WhenRaw {
    fn to_when(self) -> Option<When> {
        match self {
            WhenRaw::OnSuccess => Some(When::OnSuccess),
            WhenRaw::OnFailure => Some(When::OnFailure),
            WhenRaw::Always => Some(When::Always),
            WhenRaw::Manual => Some(When::Manual),
            WhenRaw::Never => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Rule {
    condition: Option<String>,
    when: Option<WhenRaw>,
    allow_failure: Option<bool>,
    variables: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
struct RefFilter {
    refs: Vec<String>,
    variables: Vec<String>,
}

#[derive(Debug, Clone)]
struct Need {
    job: String,
    optional: bool,
}

/// A job after `extends`, defaults and static validation, before rules.
#[derive(Debug, Clone)]
struct RawJob {
    name: String,
    stage: String,
    stage_idx: usize,
    when: Option<WhenRaw>,
    allow_failure: Option<bool>,
    needs: Option<Vec<Need>>,
    image: Option<ImageSpec>,
    services: Vec<ServiceSpec>,
    before_script: Vec<String>,
    script: Vec<String>,
    after_script: Vec<String>,
    variables: Vec<(String, String)>,
    rules: Option<Vec<Rule>>,
    only: Option<RefFilter>,
    except: Option<RefFilter>,
    tags: Vec<String>,
    environment: Option<Environment>,
    resource_group: Option<String>,
    timeout_secs: Option<u64>,
    interruptible: bool,
    retry: u32,
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

pub fn plan(src: &str, ctx: &Context) -> Result<Outcome, Error> {
    let root = match yaml::parse(src)? {
        Value::Map(m) => m,
        other => {
            return Err(invalid(format!("the file must be a mapping of jobs, got {}", other.kind())));
        }
    };

    for key in ["include", "spec"] {
        if root.contains_key(key) {
            return Err(invalid(format!("top-level `{key}` is not supported")));
        }
    }
    if root.contains_key("cache") {
        return Err(invalid("top-level `cache` is not supported"));
    }

    let stages = parse_stages(&root)?;
    let global_vars = parse_variables(root.get("variables"), "top-level `variables`")?;
    let defaults = parse_defaults(&root)?;

    let mut resolver = Resolver::new(&root);
    let mut raw_jobs = Vec::new();
    for (name, value) in root.iter() {
        if TOP_LEVEL_KEYWORDS.contains(&name) || name.starts_with('.') {
            continue;
        }
        if !matches!(value, Value::Map(_)) {
            return Err(invalid(format!("job `{name}` must be a mapping, got {}", value.kind())));
        }
        let mut map = resolver.resolve(name)?;
        for (key, value) in defaults.iter() {
            if !map.contains_key(key) {
                map.insert(key.to_string(), value.clone());
            }
        }
        raw_jobs.push(parse_job(name, &map, &stages)?);
    }
    if raw_jobs.is_empty() {
        return Err(invalid("no jobs defined (hidden `.jobs` do not run)"));
    }
    let all_names: HashSet<&str> = raw_jobs.iter().map(|j| j.name.as_str()).collect();
    for job in &raw_jobs {
        for need in job.needs.iter().flatten() {
            if !all_names.contains(need.job.as_str()) && !need.optional {
                return Err(invalid(format!("job `{}` needs `{}`, which is not defined", job.name, need.job)));
            }
        }
    }

    // workflow
    let mut pipeline_vars = global_vars.clone();
    if let Some(workflow) = root.get("workflow") {
        let workflow = workflow
            .as_map()
            .ok_or_else(|| invalid(format!("`workflow` must be a mapping, got {}", workflow.kind())))?;
        for key in workflow.keys() {
            if !matches!(key, "rules" | "name" | "auto_cancel") {
                return Err(invalid(format!("`workflow:{key}` is not supported")));
            }
        }
        if let Some(rules) = workflow.get("rules") {
            let rules = parse_rules(rules, "`workflow`")?;
            let vars = layer(&global_vars, &[], &ctx.variables);
            match first_match(&rules, &vars, "`workflow:rules`")? {
                None => {
                    return Ok(Outcome::NoPipeline { reason: "no `workflow:rules` entry matched".into() });
                }
                Some(rule) => match rule.when {
                    Some(WhenRaw::Never) => {
                        return Ok(Outcome::NoPipeline {
                            reason: "a `workflow:rules` entry with `when: never` matched".into(),
                        });
                    }
                    None | Some(WhenRaw::Always) => overlay(&mut pipeline_vars, &rule.variables),
                    Some(_) => {
                        return Err(invalid("`workflow:rules:when` must be `always` or `never`"));
                    }
                },
            }
        }
    }

    // jobs
    let mut planned: Vec<(Job, Option<Vec<Need>>)> = Vec::new();
    for raw in raw_jobs {
        let vars = layer(&pipeline_vars, &raw.variables, &ctx.variables);
        let mut job_vars = raw.variables.clone();
        let (when, allow_failure) = if let Some(rules) = &raw.rules {
            let what = format!("job `{}`", raw.name);
            let Some(rule) = first_match(rules, &vars, &what)? else {
                continue;
            };
            let Some(when) = rule.when.or(raw.when).unwrap_or(WhenRaw::OnSuccess).to_when() else {
                continue;
            };
            overlay(&mut job_vars, &rule.variables);
            // With rules, manual jobs block the pipeline unless told otherwise.
            let allow_failure = rule.allow_failure.or(raw.allow_failure).unwrap_or(false);
            (when, allow_failure)
        } else {
            let included = match &raw.only {
                Some(only) => filter_matches(only, &vars, &raw.name)?,
                None => true,
            };
            let excluded = match &raw.except {
                Some(except) => filter_matches(except, &vars, &raw.name)?,
                None => false,
            };
            if !included || excluded {
                continue;
            }
            let Some(when) = raw.when.unwrap_or(WhenRaw::OnSuccess).to_when() else {
                continue;
            };
            let allow_failure = raw.allow_failure.unwrap_or(when == When::Manual);
            (when, allow_failure)
        };
        planned.push((
            Job {
                name: raw.name,
                stage: raw.stage,
                stage_idx: raw.stage_idx,
                when,
                allow_failure,
                needs: None,
                image: raw.image,
                services: raw.services,
                before_script: raw.before_script,
                script: raw.script,
                after_script: raw.after_script,
                variables: job_vars,
                tags: raw.tags,
                environment: raw.environment,
                resource_group: raw.resource_group,
                timeout_secs: raw.timeout_secs,
                interruptible: raw.interruptible,
                retry: raw.retry,
            },
            raw.needs,
        ));
    }

    // needs against the jobs that made it into this pipeline
    let stage_of: HashMap<String, usize> = planned.iter().map(|(j, _)| (j.name.clone(), j.stage_idx)).collect();
    for (job, needs) in &mut planned {
        let Some(needs) = needs.take() else { continue };
        let mut kept = Vec::new();
        for need in needs {
            match stage_of.get(&need.job) {
                Some(&idx) if idx > job.stage_idx => {
                    return Err(invalid(format!(
                        "job `{}` needs `{}`, which is in a later stage (`{}`)",
                        job.name, need.job, stages[idx]
                    )));
                }
                Some(_) => {
                    if !kept.contains(&need.job) {
                        kept.push(need.job);
                    }
                }
                None if need.optional => {}
                None => {
                    return Err(invalid(format!(
                        "job `{}` needs `{}`, but its rules leave `{}` out of this pipeline \
                         (mark the need `optional: true` if that is expected)",
                        job.name, need.job, need.job
                    )));
                }
            }
        }
        job.needs = Some(kept);
    }
    let mut jobs: Vec<Job> = planned.into_iter().map(|(j, _)| j).collect();
    check_cycles(&jobs)?;
    jobs.sort_by_key(|j| j.stage_idx);

    let last = stages.len() - 1;
    if jobs.iter().all(|j| j.stage_idx == 0 || j.stage_idx == last) {
        // Also covers "no jobs": GitLab creates no pipeline with only .pre/.post jobs.
        return Ok(Outcome::Empty);
    }
    Ok(Outcome::Pipeline(Plan { stages, variables: pipeline_vars, jobs }))
}

/// Evaluates a `rules:if` expression against `vars` (for callers that want to
/// preview a rule). Same semantics as the planner.
pub fn evaluate_if(expression: &str, vars: &BTreeMap<String, String>) -> Result<bool, Error> {
    expr::evaluate(expression, vars).map_err(|e| invalid(format!("`{expression}`: {e}")))
}

fn layer(
    base: &[(String, String)],
    job: &[(String, String)],
    ctx: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = base.iter().cloned().collect();
    out.extend(job.iter().cloned());
    out.extend(ctx.iter().map(|(k, v)| (k.clone(), v.clone())));
    out
}

/// Replaces existing keys in place, appends new ones.
fn overlay(target: &mut Vec<(String, String)>, over: &[(String, String)]) {
    for (k, v) in over {
        match target.iter_mut().find(|(tk, _)| tk == k) {
            Some(slot) => slot.1 = v.clone(),
            None => target.push((k.clone(), v.clone())),
        }
    }
}

fn first_match<'r>(rules: &'r [Rule], vars: &BTreeMap<String, String>, what: &str) -> Result<Option<&'r Rule>, Error> {
    for rule in rules {
        let hit = match &rule.condition {
            None => true,
            Some(cond) => {
                expr::evaluate(cond, vars).map_err(|e| invalid(format!("{what}: `rules:if` `{cond}`: {e}")))?
            }
        };
        if hit {
            return Ok(Some(rule));
        }
    }
    Ok(None)
}

fn check_cycles(jobs: &[Job]) -> Result<(), Error> {
    let index: HashMap<&str, &Job> = jobs.iter().map(|j| (j.name.as_str(), j)).collect();
    // 0 = unseen, 1 = on the stack, 2 = done
    let mut state: HashMap<&str, u8> = HashMap::new();
    fn visit<'a>(
        name: &'a str,
        index: &HashMap<&'a str, &'a Job>,
        state: &mut HashMap<&'a str, u8>,
        path: &mut Vec<&'a str>,
    ) -> Result<(), Error> {
        match state.get(name).copied().unwrap_or(0) {
            2 => return Ok(()),
            1 => {
                let start = path.iter().position(|n| *n == name).unwrap_or(0);
                let mut cycle: Vec<String> = path[start..].iter().map(|n| format!("`{n}`")).collect();
                cycle.push(format!("`{name}`"));
                return Err(invalid(format!("circular `needs`: {}", cycle.join(" → "))));
            }
            _ => {}
        }
        state.insert(name, 1);
        path.push(name);
        if let Some(job) = index.get(name) {
            for need in job.needs.iter().flatten() {
                if let Some(next) = index.get(need.as_str()) {
                    visit(next.name.as_str(), index, state, path)?;
                }
            }
        }
        path.pop();
        state.insert(name, 2);
        Ok(())
    }
    for job in jobs {
        visit(job.name.as_str(), &index, &mut state, &mut Vec::new())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// only / except

fn filter_matches(filter: &RefFilter, vars: &BTreeMap<String, String>, job: &str) -> Result<bool, Error> {
    let mut refs_ok = filter.refs.is_empty();
    for r in &filter.refs {
        if ref_matches(r, vars).map_err(|e| invalid(format!("job `{job}`: only/except `{r}`: {e}")))? {
            refs_ok = true;
            break;
        }
    }
    let vars_ok = filter.variables.is_empty()
        || filter
            .variables
            .iter()
            .map(|e| {
                expr::evaluate(e, vars)
                    .map_err(|err| invalid(format!("job `{job}`: `only/except:variables` `{e}`: {err}")))
            })
            .collect::<Result<Vec<bool>, Error>>()?
            .into_iter()
            .any(|b| b);
    Ok(refs_ok && vars_ok)
}

fn ref_matches(pattern: &str, vars: &BTreeMap<String, String>) -> Result<bool, String> {
    let get = |k: &str| vars.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let tag = get("CI_COMMIT_TAG");
    let branch = get("CI_COMMIT_BRANCH");
    let ref_name = get("CI_COMMIT_REF_NAME").or(tag).or(branch).unwrap_or("");
    let source = get("CI_PIPELINE_SOURCE").unwrap_or("");
    // `pattern@group/project` limits to a project; this server has no forks.
    let pattern = pattern.split_once('@').map_or(pattern, |(p, _)| p);
    Ok(match pattern {
        "branches" => tag.is_none() && (branch.is_some() || !ref_name.is_empty()),
        "tags" => tag.is_some(),
        "api" => source == "api",
        "web" => source == "web",
        "pushes" => source == "push",
        "schedules" => source == "schedule",
        "triggers" => source == "trigger",
        "pipelines" => source == "pipeline",
        "chat" => source == "chat",
        "external" => source == "external",
        "merge_requests" => source == "merge_request_event",
        "external_pull_requests" => source == "external_pull_request_event",
        p if p.starts_with('/') && p.len() > 1 => expr::pattern_matches(p, ref_name)?,
        p => p == ref_name,
    })
}

// ---------------------------------------------------------------------------
// parsing helpers

fn parse_stages(root: &Map) -> Result<Vec<String>, Error> {
    let declared = root.get("stages").or_else(|| root.get("types"));
    let names: Vec<String> = match declared {
        None | Some(Value::Null) => vec!["build".into(), "test".into(), "deploy".into()],
        Some(Value::Seq(items)) => items
            .iter()
            .map(|v| {
                v.scalar_string()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid(format!("`stages` entries must be names, got {}", v.kind())))
            })
            .collect::<Result<_, _>>()?,
        Some(other) => {
            return Err(invalid(format!("`stages` must be a list, got {}", other.kind())));
        }
    };
    let mut stages = vec![".pre".to_string()];
    for name in names {
        if name != ".pre" && name != ".post" && !stages.contains(&name) {
            stages.push(name);
        }
    }
    stages.push(".post".into());
    Ok(stages)
}

fn parse_defaults(root: &Map) -> Result<Map, Error> {
    let mut defaults = Map::default();
    if let Some(value) = root.get("default") {
        let map =
            value.as_map().ok_or_else(|| invalid(format!("`default` must be a mapping, got {}", value.kind())))?;
        for (key, v) in map.iter() {
            if DEFAULT_KEYS.contains(&key) {
                defaults.insert(key.to_string(), v.clone());
            } else if UNSUPPORTED_DEFAULT_KEYS.contains(&key) {
                return Err(invalid(format!("`default:{key}` is not supported")));
            } else {
                return Err(invalid(format!("`default:{key}` is not a known key")));
            }
        }
    }
    for key in ["image", "services", "before_script", "after_script"] {
        if let Some(v) = root.get(key)
            && !defaults.contains_key(key)
        {
            defaults.insert(key.to_string(), v.clone());
        }
    }
    Ok(defaults)
}

fn parse_job(name: &str, map: &Map, stages: &[String]) -> Result<RawJob, Error> {
    let err = |key: &str, msg: String| invalid(format!("job `{name}`: `{key}` {msg}"));

    for key in map.keys() {
        if UNSUPPORTED_JOB_KEYS.contains(&key) {
            return Err(invalid(format!("job `{name}`: `{key}` is not supported")));
        }
        if !JOB_KEYS.contains(&key) && !IGNORED_JOB_KEYS.contains(&key) {
            return Err(invalid(format!("job `{name}`: unknown key `{key}`")));
        }
    }

    if let Some(deps) = map.get("dependencies") {
        match deps {
            Value::Seq(items) if items.is_empty() => {}
            Value::Null => {}
            _ => {
                return Err(err(
                    "dependencies",
                    "is not supported (there are no artifacts to pass between jobs)".into(),
                ));
            }
        }
    }

    let stage = match map.get("stage") {
        None | Some(Value::Null) => "test".to_string(),
        Some(v) => v.scalar_string().ok_or_else(|| err("stage", format!("must be a name, got {}", v.kind())))?,
    };
    let stage_idx = stages
        .iter()
        .position(|s| *s == stage)
        .ok_or_else(|| invalid(format!("job `{name}`: stage `{stage}` is not defined in `stages`")))?;

    let script = parse_script(map.get("script"), name, "script")?;
    if script.is_empty() {
        return Err(invalid(format!("job `{name}`: `script` is missing")));
    }

    let rules = match map.get("rules") {
        None | Some(Value::Null) => None,
        Some(v) => Some(parse_rules(v, &format!("job `{name}`"))?),
    };
    let only = map.get("only").map(|v| parse_filter(v, name, "only")).transpose()?;
    let except = map.get("except").map(|v| parse_filter(v, name, "except")).transpose()?;
    if rules.is_some() && (only.is_some() || except.is_some()) {
        return Err(invalid(format!("job `{name}`: `rules` cannot be combined with `only`/`except`")));
    }

    Ok(RawJob {
        name: name.to_string(),
        stage,
        stage_idx,
        when: map.get("when").map(|v| parse_when(v, &format!("job `{name}`"))).transpose()?,
        allow_failure: map
            .get("allow_failure")
            .map(|v| parse_allow_failure(v, &format!("job `{name}`")))
            .transpose()?,
        needs: map.get("needs").map(|v| parse_needs(v, name)).transpose()?,
        image: parse_image(map.get("image"), name)?,
        services: parse_services(map.get("services"), name)?,
        before_script: parse_script(map.get("before_script"), name, "before_script")?,
        script,
        after_script: parse_script(map.get("after_script"), name, "after_script")?,
        variables: parse_variables(map.get("variables"), &format!("job `{name}`: `variables`"))?,
        rules,
        only,
        except,
        tags: parse_string_list(map.get("tags"), name, "tags")?,
        environment: parse_environment(map.get("environment"), name)?,
        resource_group: match map.get("resource_group") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.scalar_string().ok_or_else(|| err("resource_group", format!("must be a name, got {}", v.kind())))?,
            )
            .filter(|s| !s.is_empty()),
        },
        timeout_secs: match map.get("timeout") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let text =
                    v.scalar_string().ok_or_else(|| err("timeout", format!("must be a duration, got {}", v.kind())))?;
                Some(duration::parse(&text).ok_or_else(|| err("timeout", format!("`{text}` is not a duration")))?)
            }
        },
        interruptible: match map.get("interruptible") {
            None | Some(Value::Null) => false,
            Some(v) => {
                v.as_bool().ok_or_else(|| err("interruptible", format!("must be true or false, got {}", v.kind())))?
            }
        },
        retry: parse_retry(map.get("retry"), name)?,
    })
}

fn parse_when(v: &Value, what: &str) -> Result<WhenRaw, Error> {
    match v.as_str() {
        Some("on_success") => Ok(WhenRaw::OnSuccess),
        Some("on_failure") => Ok(WhenRaw::OnFailure),
        Some("always") => Ok(WhenRaw::Always),
        Some("manual") => Ok(WhenRaw::Manual),
        Some("never") => Ok(WhenRaw::Never),
        Some("delayed") => Err(invalid(format!("{what}: `when: delayed` is not supported"))),
        _ => Err(invalid(format!("{what}: `when` must be on_success, on_failure, always, manual or never"))),
    }
}

fn parse_allow_failure(v: &Value, what: &str) -> Result<bool, Error> {
    match v {
        Value::Bool(b) => Ok(*b),
        // `{exit_codes: […]}`: some failures are allowed; without exit-code
        // filtering we treat every failure as allowed.
        Value::Map(m) if m.contains_key("exit_codes") => Ok(true),
        other => {
            Err(invalid(format!("{what}: `allow_failure` must be true, false or {{exit_codes}}, got {}", other.kind())))
        }
    }
}

fn parse_rules(v: &Value, what: &str) -> Result<Vec<Rule>, Error> {
    let items = match v {
        Value::Seq(items) => items,
        other => {
            return Err(invalid(format!("{what}: `rules` must be a list, got {}", other.kind())));
        }
    };
    items
        .iter()
        .map(|item| {
            let map = item
                .as_map()
                .ok_or_else(|| invalid(format!("{what}: each rule must be a mapping, got {}", item.kind())))?;
            for key in map.keys() {
                match key {
                    "if" | "when" | "allow_failure" | "variables" => {}
                    "changes" | "exists" | "needs" | "start_in" | "interruptible" => {
                        return Err(invalid(format!("{what}: `rules:{key}` is not supported")));
                    }
                    other => return Err(invalid(format!("{what}: unknown rule key `{other}`"))),
                }
            }
            Ok(Rule {
                condition: match map.get("if") {
                    None | Some(Value::Null) => None,
                    Some(Value::Str(s)) => Some(s.clone()),
                    Some(other) => {
                        return Err(invalid(format!("{what}: `rules:if` must be a string, got {}", other.kind())));
                    }
                },
                when: map.get("when").map(|w| parse_when(w, what)).transpose()?,
                allow_failure: map.get("allow_failure").map(|a| parse_allow_failure(a, what)).transpose()?,
                variables: parse_variables(map.get("variables"), &format!("{what}: `rules:variables`"))?,
            })
        })
        .collect()
}

fn parse_filter(v: &Value, job: &str, key: &str) -> Result<RefFilter, Error> {
    match v {
        Value::Null => Ok(RefFilter::default()),
        Value::Str(_) | Value::Seq(_) => {
            Ok(RefFilter { refs: parse_string_list(Some(v), job, key)?, variables: Vec::new() })
        }
        Value::Map(m) => {
            let mut filter = RefFilter::default();
            for (k, val) in m.iter() {
                match k {
                    "refs" => filter.refs = parse_string_list(Some(val), job, &format!("{key}:refs"))?,
                    "variables" => filter.variables = parse_string_list(Some(val), job, &format!("{key}:variables"))?,
                    "changes" | "kubernetes" => {
                        return Err(invalid(format!("job `{job}`: `{key}:{k}` is not supported")));
                    }
                    other => {
                        return Err(invalid(format!("job `{job}`: unknown key `{key}:{other}`")));
                    }
                }
            }
            Ok(filter)
        }
        other => Err(invalid(format!("job `{job}`: `{key}` must be a list or a mapping, got {}", other.kind()))),
    }
}

fn parse_needs(v: &Value, job: &str) -> Result<Vec<Need>, Error> {
    let items = match v {
        Value::Null => return Ok(Vec::new()),
        Value::Seq(items) => items.as_slice(),
        Value::Str(_) => std::slice::from_ref(v),
        other => {
            return Err(invalid(format!("job `{job}`: `needs` must be a list, got {}", other.kind())));
        }
    };
    items
        .iter()
        .map(|item| match item {
            Value::Str(s) => Ok(Need { job: s.clone(), optional: false }),
            Value::Map(m) => {
                for key in m.keys() {
                    match key {
                        "job" | "optional" | "artifacts" => {}
                        "pipeline" | "project" | "ref" | "parallel" => {
                            return Err(invalid(format!("job `{job}`: `needs:{key}` is not supported")));
                        }
                        other => {
                            return Err(invalid(format!("job `{job}`: unknown key `needs:{other}`")));
                        }
                    }
                }
                let name = m
                    .get("job")
                    .and_then(Value::scalar_string)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid(format!("job `{job}`: each `needs` entry needs a `job`")))?;
                let optional = match m.get("optional") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(b)) => *b,
                    Some(other) => {
                        return Err(invalid(format!(
                            "job `{job}`: `needs:optional` must be true or false, got {}",
                            other.kind()
                        )));
                    }
                };
                Ok(Need { job: name, optional })
            }
            other => {
                Err(invalid(format!("job `{job}`: `needs` entries must be names or mappings, got {}", other.kind())))
            }
        })
        .collect()
}

fn parse_entrypoint(v: Option<&Value>, job: &str, key: &str) -> Result<Option<Vec<String>>, Error> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let list = parse_string_list(Some(v), job, key)?;
            // `entrypoint: [""]` clears the image entrypoint.
            if list.iter().all(String::is_empty) { Ok(Some(Vec::new())) } else { Ok(Some(list)) }
        }
    }
}

fn parse_image(v: Option<&Value>, job: &str) -> Result<Option<ImageSpec>, Error> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Str(s)) if s.is_empty() => Ok(None),
        Some(Value::Str(s)) => Ok(Some(ImageSpec { name: s.clone(), entrypoint: None })),
        Some(Value::Map(m)) => {
            for key in m.keys() {
                if !matches!(key, "name" | "entrypoint" | "pull_policy" | "docker" | "kubernetes") {
                    return Err(invalid(format!("job `{job}`: unknown key `image:{key}`")));
                }
            }
            let name = m
                .get("name")
                .and_then(Value::scalar_string)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid(format!("job `{job}`: `image` needs a `name`")))?;
            Ok(Some(ImageSpec { name, entrypoint: parse_entrypoint(m.get("entrypoint"), job, "image:entrypoint")? }))
        }
        Some(other) => Err(invalid(format!("job `{job}`: `image` must be a name or a mapping, got {}", other.kind()))),
    }
}

fn parse_services(v: Option<&Value>, job: &str) -> Result<Vec<ServiceSpec>, Error> {
    let items = match v {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Seq(items)) => items,
        Some(other) => {
            return Err(invalid(format!("job `{job}`: `services` must be a list, got {}", other.kind())));
        }
    };
    items
        .iter()
        .map(|item| match item {
            Value::Str(s) => Ok(ServiceSpec {
                name: s.clone(),
                aliases: Vec::new(),
                entrypoint: None,
                command: None,
                variables: Vec::new(),
            }),
            Value::Map(m) => {
                let name = m
                    .get("name")
                    .and_then(Value::scalar_string)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid(format!("job `{job}`: each service needs a `name`")))?;
                let aliases = match m.get("alias") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(v) => v
                        .scalar_string()
                        .ok_or_else(|| invalid(format!("job `{job}`: `services:alias` must be a string")))?
                        .split(|c: char| c == ',' || c.is_whitespace())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect(),
                };
                let command = match m.get("command") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(parse_string_list(Some(v), job, "services:command")?),
                };
                let variables = parse_variables(m.get("variables"), &format!("job `{job}`: `services:variables`"))?
                    .into_iter()
                    .map(|(key, value)| Variable { key, value, file: false, masked: false })
                    .collect();
                Ok(ServiceSpec {
                    name,
                    aliases,
                    entrypoint: parse_entrypoint(m.get("entrypoint"), job, "services:entrypoint")?,
                    command,
                    variables,
                })
            }
            other => {
                Err(invalid(format!("job `{job}`: `services` entries must be names or mappings, got {}", other.kind())))
            }
        })
        .collect()
}

fn parse_environment(v: Option<&Value>, job: &str) -> Result<Option<Environment>, Error> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Str(s)) if s.is_empty() => Ok(None),
        Some(Value::Str(s)) => Ok(Some(Environment { name: s.clone(), url: None })),
        Some(Value::Map(m)) => {
            let name = m
                .get("name")
                .and_then(Value::scalar_string)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid(format!("job `{job}`: `environment` needs a `name`")))?;
            let url = m.get("url").and_then(Value::scalar_string).filter(|s| !s.is_empty());
            Ok(Some(Environment { name, url }))
        }
        Some(other) => {
            Err(invalid(format!("job `{job}`: `environment` must be a name or a mapping, got {}", other.kind())))
        }
    }
}

fn parse_retry(v: Option<&Value>, job: &str) -> Result<u32, Error> {
    let max = match v {
        None | Some(Value::Null) => return Ok(0),
        Some(Value::Int(n)) => *n,
        Some(Value::Map(m)) => match m.get("max") {
            None | Some(Value::Null) => 0,
            Some(Value::Int(n)) => *n,
            Some(other) => {
                return Err(invalid(format!("job `{job}`: `retry:max` must be 0, 1 or 2, got {}", other.kind())));
            }
        },
        Some(other) => {
            return Err(invalid(format!("job `{job}`: `retry` must be 0, 1 or 2, got {}", other.kind())));
        }
    };
    if !(0..=2).contains(&max) {
        return Err(invalid(format!("job `{job}`: `retry` must be 0, 1 or 2, got {max}")));
    }
    Ok(max as u32)
}

/// A string or a list of scalars.
fn parse_string_list(v: Option<&Value>, job: &str, key: &str) -> Result<Vec<String>, Error> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Seq(items)) => items
            .iter()
            .map(|i| {
                i.scalar_string()
                    .ok_or_else(|| invalid(format!("job `{job}`: `{key}` entries must be strings, got {}", i.kind())))
            })
            .collect(),
        Some(other) => other
            .scalar_string()
            .map(|s| vec![s])
            .ok_or_else(|| invalid(format!("job `{job}`: `{key}` must be a list, got {}", other.kind()))),
    }
}

/// A script: a string or a list, nested lists flattened (anchors of command lists).
fn parse_script(v: Option<&Value>, job: &str, key: &str) -> Result<Vec<String>, Error> {
    fn flatten(v: &Value, out: &mut Vec<String>, job: &str, key: &str) -> Result<(), Error> {
        match v {
            Value::Seq(items) => {
                for item in items {
                    flatten(item, out, job, key)?;
                }
                Ok(())
            }
            Value::Map(_) => Err(invalid(format!(
                "job `{job}`: `{key}` lines must be strings, got a mapping \
                 (quote lines that contain `: `)"
            ))),
            Value::Null => Ok(()),
            scalar => {
                out.push(scalar.scalar_string().unwrap_or_default());
                Ok(())
            }
        }
    }
    let mut out = Vec::new();
    if let Some(v) = v {
        flatten(v, &mut out, job, key)?;
    }
    Ok(out)
}

fn parse_variables(v: Option<&Value>, what: &str) -> Result<Vec<(String, String)>, Error> {
    let map = match v {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Map(m)) => m,
        Some(other) => {
            return Err(invalid(format!("{what} must be a mapping, got {}", other.kind())));
        }
    };
    map.iter()
        .map(|(key, value)| {
            let text = match value {
                Value::Map(m) => {
                    for k in m.keys() {
                        if !matches!(k, "value" | "description" | "expand" | "options") {
                            return Err(invalid(format!("{what}: `{key}` has an unknown key `{k}`")));
                        }
                    }
                    match m.get("value") {
                        None => String::new(),
                        Some(v) => v.scalar_string().ok_or_else(|| {
                            invalid(format!("{what}: `{key}:value` must be a string, got {}", v.kind()))
                        })?,
                    }
                }
                Value::Seq(_) => {
                    return Err(invalid(format!("{what}: `{key}` must be a string, got a list")));
                }
                scalar => scalar.scalar_string().unwrap_or_default(),
            };
            Ok((key.to_string(), text))
        })
        .collect()
}
