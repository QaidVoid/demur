//! Packaging checks: the composite action and the example workflow parse
//! as YAML, carry only the allowed permissions, and enforce the per-pull
//! request concurrency group.

use serde_yaml::Value;

fn load(path: &str) -> Value {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let text = std::fs::read_to_string(format!("{manifest}/../../{path}"))
        .unwrap_or_else(|err| panic!("read {path}: {err}"));
    serde_yaml::from_str(&text).unwrap_or_else(|err| panic!("{path} is not valid YAML: {err}"))
}

#[test]
fn release_workflow_builds_all_targets_and_attaches_artifacts() {
    let workflow = load(".github/workflows/release.yml");
    let matrix = &workflow["jobs"]["build"]["strategy"]["matrix"]["include"];
    let targets: Vec<&str> = matrix
        .as_sequence()
        .expect("build matrix")
        .iter()
        .map(|entry| entry["target"].as_str().expect("target"))
        .collect();
    assert!(targets.contains(&"x86_64-unknown-linux-gnu"));
    assert!(targets.contains(&"aarch64-unknown-linux-gnu"));
    assert!(targets.contains(&"aarch64-apple-darwin"));
    let release_job = &workflow["jobs"]["release"];
    assert!(
        release_job["steps"]
            .as_sequence()
            .unwrap()
            .iter()
            .any(|step| step["name"]
                .as_str()
                .is_some_and(|name| name.contains("Create the release"))),
        "release job must attach artifacts to the release"
    );
}

#[test]
fn composite_action_declares_expected_surface() {
    let action = load("action.yml");
    assert_eq!(action["runs"]["using"], "composite");
    // Inputs map to environment variables.
    let inputs = action["inputs"].as_mapping().expect("inputs table");
    assert!(inputs.contains_key(Value::from("github_token")));
    assert!(inputs.contains_key(Value::from("demur_version")));
    assert!(inputs.contains_key(Value::from("profile")));
    let runs = action["runs"]["steps"].as_sequence().expect("steps");
    let last = runs.last().unwrap();
    let env = last["env"].as_mapping().expect("step env");
    assert!(env.contains_key(Value::from("GITHUB_TOKEN")));
    assert!(env.contains_key(Value::from("DEMUR_PROFILE")));
    assert_eq!(last["shell"], "bash");
}

#[test]
fn example_workflow_limits_permissions_and_sets_concurrency() {
    let workflow = load("docs/example-workflow.yml");
    let permissions = &workflow["permissions"];
    assert_eq!(permissions["pull-requests"], "write");
    assert_eq!(permissions["checks"], "write");
    assert_eq!(permissions["contents"], "read");
    assert!(
        permissions.as_mapping().unwrap().len() == 3,
        "no other permissions"
    );

    let concurrency = &workflow["concurrency"];
    let group = concurrency["group"]
        .as_str()
        .expect("concurrency group is a string");
    assert!(group.contains("github.event.pull_request.number"));
    assert_eq!(concurrency["cancel-in-progress"], true);

    // Depending on the YAML dialect the key `on` parses as a string or as
    // the boolean true, so accept either.
    let trigger = workflow
        .get(Value::from("on"))
        .or_else(|| workflow.get(Value::Bool(true)))
        .expect("workflow `on` key missing");
    assert!(trigger.is_mapping(), "workflow `on` key missing");
}
