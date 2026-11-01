//! Two realistic, synthetic pipelines that exercise the supported subset the
//! way real projects use it: manual-only workflows, File variables, hidden
//! templates with `extends`, docker-in-docker services, `needs`, manual
//! deploys, environments and resource groups.

use std::collections::BTreeMap;

use rust_afariano_ci::{Context, Environment, ImageSpec, Job, Outcome, Plan, ServiceSpec, When, plan};

const SSH_DEPLOY: &str = include_str!("fixtures/ssh-deploy.gitlab-ci.yml");
const DOCKER_DIND: &str = include_str!("fixtures/docker-dind.gitlab-ci.yml");

fn ctx(source: &str, branch: &str) -> Context {
    let mut v = BTreeMap::new();
    v.insert("CI_PIPELINE_SOURCE".to_string(), source.to_string());
    v.insert("CI_COMMIT_BRANCH".to_string(), branch.to_string());
    v.insert("CI_COMMIT_REF_NAME".to_string(), branch.to_string());
    v.insert("CI_DEFAULT_BRANCH".to_string(), "main".to_string());
    Context { variables: v }
}

fn pipeline(src: &str, source: &str, branch: &str) -> Plan {
    match plan(src, &ctx(source, branch)).unwrap() {
        Outcome::Pipeline(p) => p,
        other => panic!("{source} on {branch}: expected a pipeline, got {other:?}"),
    }
}

fn assert_no_pipeline(src: &str, source: &str, branch: &str) {
    match plan(src, &ctx(source, branch)).unwrap() {
        Outcome::NoPipeline { reason } => assert_eq!(reason, "no `workflow:rules` entry matched"),
        other => panic!("{source} on {branch}: expected no pipeline, got {other:?}"),
    }
}

fn job<'p>(p: &'p Plan, name: &str) -> &'p Job {
    p.jobs.iter().find(|j| j.name == name).unwrap_or_else(|| panic!("job {name} missing"))
}

fn names(p: &Plan) -> Vec<&str> {
    p.jobs.iter().map(|j| j.name.as_str()).collect()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// ---------------------------------------------------------------------------
// ssh-deploy

#[test]
fn ssh_deploy_push_starts_nothing() {
    assert_no_pipeline(SSH_DEPLOY, "push", "main");
}

#[test]
fn ssh_deploy_web_and_api_on_main() {
    for source in ["web", "api"] {
        let p = pipeline(SSH_DEPLOY, source, "main");
        assert_eq!(p.stages, strings(&[".pre", "build", "test", "deploy", ".post"]));
        assert_eq!(p.variables, vec![("GIT_LFS_SKIP_SMUDGE".to_string(), "1".to_string())]);
        assert_eq!(names(&p), vec!["site:deploy"]);

        let j = job(&p, "site:deploy");
        // No `stage:` → GitLab's default `test`.
        assert_eq!(j.stage, "test");
        assert_eq!(j.stage_idx, 2);
        assert_eq!(j.when, When::OnSuccess);
        assert!(!j.allow_failure);
        assert_eq!(j.needs, None);
        assert_eq!(j.image, Some(ImageSpec { name: "alpine:3".into(), entrypoint: None }));
        assert!(j.services.is_empty());
        assert!(j.tags.is_empty());
        assert!(j.variables.is_empty());
        assert_eq!(
            j.environment,
            Some(Environment { name: "production".into(), url: Some("https://www.example.com".into()) })
        );
        assert_eq!(j.resource_group.as_deref(), Some("site"));
        assert_eq!(j.timeout_secs, Some(1800));
        assert_eq!(j.before_script, strings(&["apk add --no-cache openssh-client >/dev/null"]));
        assert_eq!(j.script.len(), 3);
        assert!(j.script[0].starts_with("grep -q 'PRIVATE KEY' \"$DEPLOY_SSH_KEY\""));
        assert_eq!(j.script[1], "chmod 600 \"$DEPLOY_SSH_KEY\"");
        assert!(j.script[2].ends_with("deploy@203.0.113.10 \"$CI_COMMIT_SHA\""));
        assert!(j.after_script.is_empty());
        assert_eq!(j.retry, 0);
        assert!(!j.interruptible);
    }
}

#[test]
fn ssh_deploy_other_branches_start_nothing() {
    assert_no_pipeline(SSH_DEPLOY, "web", "staging");
    assert_no_pipeline(SSH_DEPLOY, "web", "feature/x");
}

// ---------------------------------------------------------------------------
// docker-dind

fn docker_image() -> Option<ImageSpec> {
    Some(ImageSpec { name: "docker:29-cli".into(), entrypoint: None })
}

fn dind() -> Vec<ServiceSpec> {
    vec![ServiceSpec {
        name: "docker:29-dind".into(),
        aliases: vec![],
        entrypoint: None,
        command: None,
        variables: vec![],
    }]
}

#[test]
fn docker_dind_push_starts_nothing() {
    assert_no_pipeline(DOCKER_DIND, "push", "main");
    assert_no_pipeline(DOCKER_DIND, "push", "staging");
}

#[test]
fn docker_dind_web_and_api_on_main() {
    for source in ["web", "api"] {
        let p = pipeline(DOCKER_DIND, source, "main");
        assert_eq!(p.stages, strings(&[".pre", "test", "deploy", ".post"]));
        assert!(p.variables.is_empty());
        assert_eq!(names(&p), vec!["production:test", "production:deploy"]);

        let test = job(&p, "production:test");
        assert_eq!((test.stage.as_str(), test.stage_idx), ("test", 1));
        assert_eq!(test.when, When::OnSuccess);
        assert!(!test.allow_failure);
        assert_eq!(test.needs, None);
        // inherited from `.docker`
        assert_eq!(test.image, docker_image());
        assert_eq!(test.services, dind());
        assert_eq!(test.tags, strings(&["docker-build"]));
        assert_eq!(test.variables, vec![("DOCKER_TLS_CERTDIR".to_string(), "/certs".to_string())]);
        assert_eq!(test.before_script, strings(&["apk add --no-cache bash git openssh-client >/dev/null"]));
        assert_eq!(test.script, strings(&["bash ci/production-test.sh"]));
        assert_eq!(test.timeout_secs, Some(3600));
        assert_eq!(test.environment, None);
        assert_eq!(test.resource_group, None);

        let deploy = job(&p, "production:deploy");
        assert_eq!((deploy.stage.as_str(), deploy.stage_idx), ("deploy", 2));
        // `when: manual` from rules: blocks the pipeline (allow_failure false).
        assert_eq!(deploy.when, When::Manual);
        assert!(!deploy.allow_failure);
        assert_eq!(deploy.needs, Some(strings(&["production:test"])));
        assert_eq!(deploy.image, docker_image());
        assert_eq!(deploy.services, dind());
        assert_eq!(deploy.tags, strings(&["docker-build"]));
        assert_eq!(deploy.variables, vec![("DOCKER_TLS_CERTDIR".to_string(), "/certs".to_string())]);
        assert_eq!(deploy.before_script, strings(&["apk add --no-cache bash git openssh-client >/dev/null"]));
        assert_eq!(deploy.resource_group.as_deref(), Some("production"));
        assert_eq!(deploy.environment, Some(Environment { name: "production".into(), url: None }));
        assert_eq!(deploy.timeout_secs, Some(7200));
        assert_eq!(deploy.script, strings(&["bash ci/production-deploy.sh"]));
    }
}

#[test]
fn docker_dind_web_on_staging() {
    let p = pipeline(DOCKER_DIND, "web", "staging");
    assert_eq!(names(&p), vec!["staging:deploy"]);
    let j = job(&p, "staging:deploy");
    assert_eq!((j.stage.as_str(), j.stage_idx), ("deploy", 2));
    assert_eq!(j.when, When::OnSuccess);
    assert!(!j.allow_failure);
    assert_eq!(j.needs, None);
    assert_eq!(j.image, Some(ImageSpec { name: "alpine:3".into(), entrypoint: None }));
    assert!(j.services.is_empty());
    assert!(j.tags.is_empty());
    assert!(j.variables.is_empty());
    assert_eq!(j.resource_group.as_deref(), Some("staging"));
    assert_eq!(
        j.environment,
        Some(Environment { name: "staging".into(), url: Some("https://staging.example.com".into()) })
    );
    assert_eq!(j.timeout_secs, Some(2700));
    assert_eq!(j.before_script, strings(&["apk add --no-cache bash git openssh-client >/dev/null"]));
    assert_eq!(j.script, strings(&["bash ci/staging-deploy.sh"]));
}

#[test]
fn docker_dind_feature_branch_starts_nothing() {
    assert_no_pipeline(DOCKER_DIND, "web", "feature/x");
    assert_no_pipeline(DOCKER_DIND, "api", "feature/x");
}
