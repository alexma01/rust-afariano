use std::collections::BTreeMap;

use rust_afariano_ci::{Context, Error, Job, Outcome, Plan, When, evaluate_if, parse_duration, plan};

fn ctx_with(pairs: &[(&str, &str)]) -> Context {
    let mut v: BTreeMap<String, String> = [
        ("CI_PIPELINE_SOURCE", "push"),
        ("CI_COMMIT_BRANCH", "main"),
        ("CI_COMMIT_REF_NAME", "main"),
        ("CI_DEFAULT_BRANCH", "main"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, val) in pairs {
        if val.is_empty() {
            v.remove(*k);
        } else {
            v.insert(k.to_string(), val.to_string());
        }
    }
    Context { variables: v }
}

fn tag_ctx(tag: &str) -> Context {
    ctx_with(&[("CI_COMMIT_BRANCH", ""), ("CI_COMMIT_TAG", tag), ("CI_COMMIT_REF_NAME", tag)])
}

fn ok(src: &str) -> Plan {
    ok_in(src, &ctx_with(&[]))
}

fn ok_in(src: &str, ctx: &Context) -> Plan {
    match plan(src, ctx) {
        Ok(Outcome::Pipeline(p)) => p,
        other => panic!("expected a pipeline, got {other:?}"),
    }
}

fn err(src: &str) -> String {
    match plan(src, &ctx_with(&[])) {
        Err(Error::Invalid(m)) | Err(Error::Yaml(m)) => m,
        other => panic!("expected an error, got {other:?}"),
    }
}

fn job<'p>(p: &'p Plan, name: &str) -> &'p Job {
    p.jobs.iter().find(|j| j.name == name).unwrap_or_else(|| panic!("no job {name}"))
}

fn names(p: &Plan) -> Vec<&str> {
    p.jobs.iter().map(|j| j.name.as_str()).collect()
}

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|x| x.to_string()).collect()
}

// ---------------------------------------------------------------------------
// inheritance

#[test]
fn default_and_legacy_globals() {
    let p = ok(r#"
image: legacy:1
before_script: [legacy-before]
after_script: legacy-after
default:
  image: default:1
  tags: [t]
  timeout: 10m
  retry: 2
  interruptible: true
a:
  script: a
b:
  image: own:1
  before_script: []
  tags: []
  script: b
"#);
    let a = job(&p, "a");
    assert_eq!(a.image.as_ref().unwrap().name, "default:1");
    assert_eq!(a.before_script, s(&["legacy-before"]));
    assert_eq!(a.after_script, s(&["legacy-after"]));
    assert_eq!(a.tags, s(&["t"]));
    assert_eq!(a.timeout_secs, Some(600));
    assert_eq!(a.retry, 2);
    assert!(a.interruptible);

    let b = job(&p, "b");
    assert_eq!(b.image.as_ref().unwrap().name, "own:1");
    assert!(b.before_script.is_empty());
    assert!(b.tags.is_empty());
}

#[test]
fn hidden_jobs_extends_anchors_merge_keys() {
    let p = ok(r#"
.common: &common
  image: alpine
  variables: {A: "1", B: "1"}
  before_script: &setup
    - apk add git
.more:
  extends: .common
  variables: {B: "2"}
  tags: [x]
build:
  <<: *common
  stage: build
  script:
    - *setup
    - make
test:
  extends: [.more]
  variables: {C: "3"}
  script: [make test]
"#);
    assert_eq!(names(&p), vec!["build", "test"]);
    let build = job(&p, "build");
    assert_eq!(build.image.as_ref().unwrap().name, "alpine");
    // nested script anchors are flattened
    assert_eq!(build.script, s(&["apk add git", "make"]));
    let test = job(&p, "test");
    assert_eq!(
        test.variables,
        vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
            ("C".to_string(), "3".to_string())
        ]
    );
    assert_eq!(test.tags, s(&["x"]));
    assert_eq!(test.before_script, s(&["apk add git"]));
}

#[test]
fn order_is_stage_then_file() {
    let p = ok(r#"
stages: [one, two]
z2: {stage: two, script: x}
a1: {stage: one, script: x}
b2: {stage: two, script: x}
pre: {stage: .pre, script: x}
post: {stage: .post, script: x}
c1: {stage: one, script: x}
"#);
    assert_eq!(p.stages, s(&[".pre", "one", "two", ".post"]));
    assert_eq!(names(&p), vec!["pre", "a1", "c1", "z2", "b2", "post"]);
}

// ---------------------------------------------------------------------------
// when / allow_failure / rules

#[test]
fn manual_allow_failure_defaults() {
    let p = ok(r#"
job_level: {script: x, when: manual}
rules_level: {script: x, rules: [{when: manual}]}
rules_inherit: {script: x, when: manual, rules: [{if: $CI_COMMIT_BRANCH}]}
explicit: {script: x, rules: [{when: manual, allow_failure: true}]}
job_explicit: {script: x, when: manual, allow_failure: false}
always: {script: x, when: always}
"#);
    assert_eq!((job(&p, "job_level").when, job(&p, "job_level").allow_failure), (When::Manual, true));
    assert_eq!((job(&p, "rules_level").when, job(&p, "rules_level").allow_failure), (When::Manual, false));
    assert_eq!((job(&p, "rules_inherit").when, job(&p, "rules_inherit").allow_failure), (When::Manual, false));
    assert!(job(&p, "explicit").allow_failure);
    assert!(!job(&p, "job_explicit").allow_failure);
    assert_eq!(job(&p, "always").when, When::Always);
}

#[test]
fn rules_select_and_layer_variables() {
    let src = r#"
variables: {GLOBAL: g, MODE: global}
job:
  script: x
  variables: {MODE: job, KEEP: k}
  rules:
    - if: $MODE == "ctx"
      variables: {PICKED: ctx-rule}
    - if: $MODE == "job" && $GLOBAL == "g"
      when: on_failure
      variables: {MODE: from-rule, PICKED: job-rule}
    - when: never
other:
  rules:
    - if: $NOPE
  script: x
never:
  script: x
  rules:
    - when: never
filler:
  script: x
"#;
    let p = ok(src);
    assert_eq!(names(&p), vec!["job", "filler"]);
    let j = job(&p, "job");
    assert_eq!(j.when, When::OnFailure);
    assert_eq!(
        j.variables,
        vec![
            ("MODE".to_string(), "from-rule".to_string()),
            ("KEEP".to_string(), "k".to_string()),
            ("PICKED".to_string(), "job-rule".to_string())
        ]
    );
    // context variables win over YAML variables in rules
    let p = ok_in(src, &ctx_with(&[("MODE", "ctx")]));
    assert_eq!(job(&p, "job").variables.last().unwrap(), &("PICKED".to_string(), "ctx-rule".to_string()));
}

#[test]
fn workflow_rules() {
    let src = r#"
variables: {DEPLOY: "no"}
workflow:
  rules:
    - if: $CI_COMMIT_TAG
      when: never
    - if: $CI_PIPELINE_SOURCE == "push"
      variables: {DEPLOY: "yes", EXTRA: "1"}
    - if: $CI_PIPELINE_SOURCE == "web"
job:
  script: x
  rules:
    - if: $DEPLOY == "yes"
"#;
    let p = ok(src);
    assert_eq!(p.variables, vec![("DEPLOY".to_string(), "yes".to_string()), ("EXTRA".to_string(), "1".to_string())]);
    assert_eq!(names(&p), vec!["job"]);

    assert_eq!(
        plan(src, &tag_ctx("v1")).unwrap(),
        Outcome::NoPipeline { reason: "a `workflow:rules` entry with `when: never` matched".into() }
    );
    // web: workflow ok, but the job's rule fails → no jobs
    assert_eq!(plan(src, &ctx_with(&[("CI_PIPELINE_SOURCE", "web")])).unwrap(), Outcome::Empty);
    assert!(matches!(plan(src, &ctx_with(&[("CI_PIPELINE_SOURCE", "schedule")])).unwrap(), Outcome::NoPipeline { .. }));
    assert!(err("workflow:\n  rules:\n    - when: manual\njob: {script: x}\n").contains("always"));
}

#[test]
fn only_pre_and_post_is_empty() {
    let src = "pre: {stage: .pre, script: x}\nreal: {script: x, rules: [{if: $NOPE}]}\n";
    assert_eq!(plan(src, &ctx_with(&[])).unwrap(), Outcome::Empty);
}

#[test]
fn only_except() {
    let src = r#"
tags_only: {script: x, only: [tags]}
release: {script: x, only: ['/^release\/.*$/i']}
not_main: {script: x, except: [main]}
branches: {script: x, only: [branches]}
web_only: {script: x, only: [web]}
var_only: {script: x, only: {refs: [branches], variables: ['$FLAG == "1"']}}
plain: {script: x}
"#;
    let p = ok(src);
    assert_eq!(names(&p), vec!["branches", "plain"]);

    let p = ok_in(src, &tag_ctx("v1.0"));
    assert_eq!(names(&p), vec!["tags_only", "not_main", "plain"]);

    let p =
        ok_in(src, &ctx_with(&[("CI_COMMIT_BRANCH", "Release/2"), ("CI_COMMIT_REF_NAME", "Release/2"), ("FLAG", "1")]));
    assert_eq!(names(&p), vec!["release", "not_main", "branches", "var_only", "plain"]);

    let p = ok_in(src, &ctx_with(&[("CI_PIPELINE_SOURCE", "web")]));
    assert!(names(&p).contains(&"web_only"));
}

// ---------------------------------------------------------------------------
// needs

#[test]
fn needs_rules() {
    let p = ok(r#"
stages: [a, b]
first: {stage: a, script: x}
same: {stage: a, script: x, needs: [first]}
gone: {stage: a, script: x, rules: [{when: never}]}
second:
  stage: b
  script: x
  needs: [first, {job: gone, optional: true}, {job: first}]
free: {stage: b, script: x, needs: []}
"#);
    assert_eq!(job(&p, "second").needs, Some(s(&["first"])));
    assert_eq!(job(&p, "same").needs, Some(s(&["first"])));
    assert_eq!(job(&p, "free").needs, Some(vec![]));
    assert_eq!(job(&p, "first").needs, None);

    assert!(err("a: {script: x, needs: [nope]}\n").contains("`nope`, which is not defined"));
    assert!(
        err("stages: [a, b]\nx: {stage: a, script: x, needs: [y]}\ny: {stage: b, script: x}\n").contains("later stage")
    );
    assert!(err("x: {script: x, needs: [y]}\ny: {script: x, needs: [x]}\n").contains("circular `needs`"));
    assert!(err("x: {script: x, needs: [x]}\n").contains("circular `needs`"));
    assert!(err("x: {script: x, needs: [y]}\ny: {script: x, rules: [{if: $NOPE}]}\n").contains("optional: true"));
    assert!(err("x: {script: x, needs: [{job: y, project: other}]}\ny: {script: x}\n").contains("needs:project"));
}

// ---------------------------------------------------------------------------
// images, services, misc fields

#[test]
fn image_and_services_forms() {
    let p = ok(r#"
job:
  image: {name: "alpine/git", entrypoint: [""]}
  services:
    - postgres:16
    - name: registry.example.com/team/cache:2
      alias: cache, kv
      command: ["--port", "6380"]
      entrypoint: ["/bin/run"]
      variables: {MODE: fast}
  script: x
  environment: review
  allow_failure: {exit_codes: [3]}
  retry: {max: 1, when: [script_failure]}
  timeout: 1h 30m
"#);
    let j = job(&p, "job");
    let image = j.image.as_ref().unwrap();
    assert_eq!(image.name, "alpine/git");
    assert_eq!(image.entrypoint, Some(vec![]));
    assert_eq!(j.services[0].name, "postgres:16");
    assert!(j.services[0].aliases.is_empty());
    let svc = &j.services[1];
    assert_eq!(svc.aliases, s(&["cache", "kv"]));
    assert_eq!(svc.command, Some(s(&["--port", "6380"])));
    assert_eq!(svc.entrypoint, Some(s(&["/bin/run"])));
    assert_eq!(svc.variables[0].key, "MODE");
    assert_eq!(svc.variables[0].value, "fast");
    assert_eq!(j.environment.as_ref().unwrap().name, "review");
    assert!(j.allow_failure);
    assert_eq!(j.retry, 1);
    assert_eq!(j.timeout_secs, Some(5400));
}

#[test]
fn variable_forms() {
    let p = ok(r#"
variables:
  NUM: 42
  FLOAT: 1.10
  BOOL: true
  DESC: {value: "v", description: "shown in the run form"}
  PROMPT: {description: "no default"}
job: {script: [echo $NUM, 3]}
"#);
    assert_eq!(
        p.variables,
        vec![
            ("NUM".to_string(), "42".to_string()),
            ("FLOAT".to_string(), "1.10".to_string()),
            ("BOOL".to_string(), "true".to_string()),
            ("DESC".to_string(), "v".to_string()),
            ("PROMPT".to_string(), String::new()),
        ]
    );
    assert_eq!(job(&p, "job").script, s(&["echo $NUM", "3"]));
}

// ---------------------------------------------------------------------------
// errors

#[test]
fn static_errors() {
    let cases: &[(&str, &str)] = &[
        ("job: {stage: nope, script: x}\n", "stage `nope` is not defined"),
        ("stages: [build]\njob: {script: x}\n", "stage `test` is not defined"),
        ("job: {image: alpine}\n", "`script` is missing"),
        ("job: {script: x, artifacts: {paths: [a]}}\n", "job `job`: `artifacts` is not supported"),
        ("job: {script: x, cache: {paths: [a]}}\n", "job `job`: `cache` is not supported"),
        ("job: {script: x, parallel: 2}\n", "job `job`: `parallel` is not supported"),
        ("job: {trigger: other/project}\n", "job `job`: `trigger` is not supported"),
        ("job: {script: x, dependencies: [a]}\n", "`dependencies` is not supported"),
        ("include: other.yml\njob: {script: x}\n", "top-level `include` is not supported"),
        ("cache: {paths: [a]}\njob: {script: x}\n", "top-level `cache` is not supported"),
        ("default: {artifacts: {}}\njob: {script: x}\n", "`default:artifacts` is not supported"),
        ("job: {script: x, colour: red}\n", "job `job`: unknown key `colour`"),
        ("job: {script: x, rules: [{changes: [a]}]}\n", "`rules:changes` is not supported"),
        ("job: {script: x, rules: [{exists: [a]}]}\n", "`rules:exists` is not supported"),
        ("job: {script: x, rules: [{when: always}], only: [main]}\n", "cannot be combined"),
        ("job: {script: x, when: delayed}\n", "`when: delayed` is not supported"),
        ("job: {script: x, when: sometimes}\n", "`when` must be"),
        ("job: {script: x, timeout: soon}\n", "`soon` is not a duration"),
        ("job: {script: x, retry: 5}\n", "`retry` must be 0, 1 or 2"),
        ("job: {script: [{a: b}]}\n", "lines must be strings"),
        ("job: {script: x, rules: [{if: '$A =='}]}\n", "`rules:if` `$A ==`"),
        ("job: [1, 2]\n", "job `job` must be a mapping"),
        ("job: {extends: .nope, script: x}\n", "`extends` target `.nope` is not defined"),
        (".a: {extends: .b}\n.b: {extends: .a}\njob: {extends: .a, script: x}\n", "circular `extends`"),
        (".hidden: {script: x}\n", "no jobs defined"),
        ("", "no jobs defined"),
        ("- a\n- b\n", "must be a mapping of jobs"),
    ];
    for (src, needle) in cases {
        let message = err(src);
        assert!(message.contains(needle), "{src:?}: {message:?} should mention {needle:?}");
    }
    assert!(matches!(plan("job: {script: [x\n", &ctx_with(&[])), Err(Error::Yaml(_))));
    // `dependencies: []` only says "download nothing" — harmless.
    ok("job: {script: x, dependencies: []}\n");
    // ignored keys
    ok("job: {script: x, coverage: '/\\d+%/'}\n");
}

#[test]
fn errors_do_not_depend_on_rules() {
    // A broken job is reported even when its rules would leave it out.
    let msg = err("job: {script: x, rules: [{if: $NOPE}], artifacts: {}}\nok: {script: x}\n");
    assert!(msg.contains("`artifacts` is not supported"), "{msg}");
}

// ---------------------------------------------------------------------------
// public helpers

#[test]
fn public_helpers() {
    assert_eq!(parse_duration("2 hours"), Some(7200));
    assert_eq!(parse_duration("nope"), None);
    let vars: BTreeMap<String, String> = [("A".to_string(), "x".to_string())].into();
    assert_eq!(evaluate_if(r#"$A == "x""#, &vars), Ok(true));
    assert!(evaluate_if("$A ==", &vars).is_err());
}

#[test]
fn form_variables_are_the_described_ones() {
    let src = r#"
variables:
  PLAIN: x
  NO_DESCRIPTION:
    value: y
  APP_BRANCH:
    value: development
    description: Branch to build.
  APP_ENV:
    value: dev
    options: [dev, test, prod]
    description: Environment.
  EMPTY:
    description: No default.
job:
  script: [echo]
"#;
    let vars = rust_afariano_ci::form_variables(src).unwrap();
    let got: Vec<(&str, &str, &str, Vec<&str>)> = vars
        .iter()
        .map(|v| {
            (v.key.as_str(), v.value.as_str(), v.description.as_str(), v.options.iter().map(String::as_str).collect())
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("APP_BRANCH", "development", "Branch to build.", vec![]),
            ("APP_ENV", "dev", "Environment.", vec!["dev", "test", "prod"]),
            ("EMPTY", "", "No default.", vec![]),
        ]
    );
    // The plan still sees every variable with its value.
    assert!(ok(src).variables.contains(&("APP_ENV".into(), "dev".into())));
    assert!(rust_afariano_ci::form_variables("job:\n  script: [echo]\n").unwrap().is_empty());
}

#[test]
fn variable_value_must_be_one_of_its_options() {
    let e = err("variables:\n  ENV:\n    value: qa\n    options: [dev, prod]\njob:\n  script: [echo]\n");
    assert!(e.contains("`ENV:value` must be one of its `options`, got `qa`"), "{e}");
    let e = err("variables:\n  ENV:\n    value: dev\n    options: dev\njob:\n  script: [echo]\n");
    assert!(e.contains("`ENV:options` must be a list"), "{e}");
    assert!(
        rust_afariano_ci::form_variables("variables:\n  ENV:\n    value: qa\n    options: [dev]\n    description: d\n")
            .is_err()
    );
}
