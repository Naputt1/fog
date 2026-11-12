//! `fog.json` is written by hand and, increasingly, by coding agents, so a
//! misspelled key must fail loudly instead of silently dropping a health check
//! or a dependency. These tests cover both halves of that promise: unknown keys
//! are rejected at parse time, and cross references are checked by
//! `Config::validate` before anything starts.

use fog::config::{Config, ConfigIssue, HealthCheckSpec};
use serde_json::Value;

fn config(source: &str) -> Config {
    serde_json::from_str(source).expect("config should deserialize")
}

fn issues_for(source: &str) -> Vec<ConfigIssue> {
    config(source).validate()
}

fn assert_issue(source: &str, path: &str, fragment: &str) {
    let issues = issues_for(source);
    assert!(
        issues
            .iter()
            .any(|issue| issue.path == path && issue.message.contains(fragment)),
        "expected an issue at {path} mentioning {fragment:?}, got {issues:?}"
    );
}

fn assert_valid(source: &str) {
    let issues = issues_for(source);
    assert!(issues.is_empty(), "expected no issues, got {issues:?}");
}

// ---------------------------------------------------------------------------
// Unknown keys
// ---------------------------------------------------------------------------

#[test]
fn misspelled_key_is_rejected_and_named() {
    let error = serde_json::from_str::<Config>(
        r#"{"scripts":{"dev":{"service":[{"path":".","cmd":"true","healt_check":{"kind":"tcp","target":"localhost:5432"}}]}}}"#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("healt_check"), "{error}");
}

#[test]
fn misspelled_top_level_key_is_rejected_and_named() {
    let error = serde_json::from_str::<Config>(r#"{"scripts":{},"nativeroutes":[]}"#)
        .unwrap_err()
        .to_string();
    assert!(error.contains("nativeroutes"), "{error}");
}

#[test]
fn misspelled_key_inside_health_check_is_rejected_and_named() {
    // `HealthCheckSpec` dispatches on the JSON shape instead of using an
    // untagged enum, which keeps the inner `unknown field` error rather than
    // collapsing it to "did not match any variant" (which names no key).
    let error = serde_json::from_str::<Config>(
        r#"{"scripts":{"dev":{"service":[{"path":".","cmd":"true","health_check":{"kind":"tcp","target":"x","healt":1}}]}}}"#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("healt"), "{error}");
}

#[test]
fn misspelled_nested_key_is_rejected_and_named() {
    let error = serde_json::from_str::<Config>(r##"{"theme":{"background":"#000000"}}"##)
        .unwrap_err()
        .to_string();
    assert!(error.contains("background"), "{error}");
}

#[test]
fn dollar_schema_key_is_accepted() {
    let config = config(
        r#"{"$schema":"https://raw.githubusercontent.com/Naputt1/fog/main/fog.schema.json","scripts":{"dev":{}}}"#,
    );
    assert_eq!(
        config.schema.as_deref(),
        Some("https://raw.githubusercontent.com/Naputt1/fog/main/fog.schema.json")
    );
    assert!(config.validate().is_empty());
}

#[test]
fn single_object_health_check_still_parses() {
    let config = config(
        r#"{"scripts":{"dev":{"service":[{"path":".","cmd":"true","health_check":{"kind":"tcp","target":"localhost:5432"}}]}}}"#,
    );
    let entry = &config.scripts["dev"].service.as_ref().unwrap()[0];
    assert!(matches!(
        entry.health_check,
        Some(HealthCheckSpec::Single(_))
    ));
}

#[test]
fn array_health_check_still_parses() {
    let config = config(
        r#"{"scripts":{"dev":{"service":[{"path":".","cmd":"true","health_check":[{"kind":"tcp","target":"localhost:5432"},{"kind":"http","target":"localhost:8080/health"}]}]}}}"#,
    );
    let entry = &config.scripts["dev"].service.as_ref().unwrap()[0];
    assert!(matches!(
        entry.health_check,
        Some(HealthCheckSpec::Multiple(ref checks)) if checks.len() == 2
    ));
}

// ---------------------------------------------------------------------------
// Semantic validation
// ---------------------------------------------------------------------------

#[test]
fn empty_cmd_is_an_issue() {
    assert_issue(
        r#"{"scripts":{"dev":{"service":[{"path":".","cmd":"  "}]}}}"#,
        "scripts.dev.service[0].cmd",
        "cmd must not be empty",
    );
}

#[test]
fn duplicate_service_names_are_an_issue() {
    assert_issue(
        r#"{"scripts":{"dev":{"service":[{"name":"db","path":".","cmd":"true"},{"name":"db","path":"other","cmd":"true"}]}}}"#,
        "scripts.dev.service[1]",
        "duplicate service name 'db'",
    );
}

#[test]
fn duplicate_names_from_paths_are_an_issue() {
    assert_issue(
        r#"{"scripts":{"dev":{"service":[{"path":"apps/api","cmd":"true"},{"path":"libs/api","cmd":"true"}]}}}"#,
        "scripts.dev.service[1]",
        "duplicate service name 'api'",
    );
}

#[test]
fn unknown_depends_on_is_an_issue() {
    assert_issue(
        r#"{"scripts":{"dev":{"service":[{"name":"api","path":".","cmd":"true","depends_on":["db2"]}]}}}"#,
        "scripts.dev.service[0].depends_on",
        "service 'api' depends on unknown service 'db2'",
    );
}

#[test]
fn dependency_cycle_is_an_issue() {
    assert_issue(
        r#"{"scripts":{"dev":{"service":[
            {"name":"a","path":".","cmd":"true","depends_on":["b"]},
            {"name":"b","path":".","cmd":"true","depends_on":["a"]}
        ]}}}"#,
        "scripts.dev.service",
        "circular dependency",
    );
}

#[test]
fn validation_collects_unknown_dependency_and_cycle_together() {
    let issues = issues_for(
        r#"{"scripts":{"dev":{"service":[
            {"name":"a","path":".","cmd":"true","depends_on":["b","ghost"]},
            {"name":"b","path":".","cmd":"true","depends_on":["c"]},
            {"name":"c","path":".","cmd":"true","depends_on":["b"]}
        ]}}}"#,
    );
    assert!(
        issues
            .iter()
            .any(|issue| issue.message.contains("unknown service 'ghost'")),
        "{issues:?}"
    );
    assert!(
        issues
            .iter()
            .any(|issue| issue.message.contains("circular dependency")),
        "{issues:?}"
    );
}

#[test]
fn share_without_health_check_is_an_issue() {
    assert_issue(
        r#"{"scripts":{"dev":{"service":[{"name":"db","path":".","cmd":"true","share":true}]}}}"#,
        "scripts.dev.service[0].share",
        "needs a health_check",
    );
}

#[test]
fn reuse_without_health_check_is_an_issue() {
    assert_issue(
        r#"{"scripts":{"dev":{"service":[{"name":"db","path":".","cmd":"true","reuse":true}]}}}"#,
        "scripts.dev.service[0].reuse",
        "needs a health_check",
    );
}

#[test]
fn share_with_an_endpoint_health_check_is_fine() {
    assert_valid(
        r#"{"scripts":{"dev":{"service":[{"name":"db","path":".","cmd":"true","share":true,
            "endpoint":[{"name":"pg","health_check":{"kind":"tcp","target":"localhost:5432"}}]}]}}}"#,
    );
}

#[test]
fn reserved_characters_in_port_names_are_an_issue() {
    assert_issue(
        r#"{"ports":{"api@my-repo:main":0},"scripts":{"dev":{}}}"#,
        "ports.api@my-repo:main",
        "must not contain '@' or ':'",
    );
}

#[test]
fn zero_port_entry_is_allowed() {
    assert_valid(r#"{"ports":{"api":0,"web":8080},"scripts":{"dev":{}}}"#);
}

#[test]
fn native_route_unknown_port_template_is_an_issue() {
    assert_issue(
        r#"{"ports":{"api":0},"scripts":{"dev":{}},"native_routes":[{"host":"api.acme","service":"api","port":"${ports.missing}"}]}"#,
        "native_routes[0]",
        "unknown port 'missing'",
    );
}

#[test]
fn native_route_invalid_literal_port_is_an_issue() {
    assert_issue(
        r#"{"scripts":{"dev":{}},"native_routes":[{"host":"api.acme","service":"api","port":"http"}]}"#,
        "native_routes[0]",
        "invalid port",
    );
}

#[test]
fn endpoint_duplicate_name_is_an_issue() {
    assert_issue(
        r#"{"ports":{"web":0},"scripts":{"dev":{"service":[{"path":".","cmd":"true","endpoint":[
            {"name":"web","port":"${ports.web}"},
            {"name":"web","port":"${ports.web}"}
        ]}]}}}"#,
        "scripts.dev",
        "duplicate endpoint name 'web'",
    );
}

#[test]
fn endpoint_unresolvable_port_is_an_issue() {
    assert_issue(
        r#"{"scripts":{"dev":{"service":[{"path":".","cmd":"true","endpoint":[{"name":"web","port":"not-a-port"}]}]}}}"#,
        "scripts.dev",
        "must be a '${ports.<name>}' template or literal port",
    );
}

#[test]
fn unknown_port_template_in_cmd_is_an_issue() {
    assert_issue(
        r#"{"ports":{"api":0},"scripts":{"dev":{"service":[{"path":".","cmd":"npm --port ${ports.db}"}]}}}"#,
        "scripts.dev.service[0].cmd",
        "unknown port 'db'",
    );
}

#[test]
fn unknown_port_template_in_env_is_an_issue() {
    assert_issue(
        r#"{"ports":{"api":0},"scripts":{"dev":{"service":[{"path":".","cmd":"true","env":{"API_PORT":"${ports.db}"}}]}}}"#,
        "scripts.dev.service[0].env.API_PORT",
        "unknown port 'db'",
    );
}

#[test]
fn unknown_template_atom_is_an_issue() {
    // `${nope}` is only recognized as part of a fog template because the same
    // string also contains a real one; that is exactly the runtime rule.
    assert_issue(
        r#"{"ports":{"api":0},"scripts":{"dev":{"service":[{"path":".","cmd":"curl localhost:${ports.api}/${nope}"}]}}}"#,
        "scripts.dev.service[0].cmd",
        "unknown template '${nope}'",
    );
}

#[test]
fn shell_expansions_are_not_templates() {
    assert_valid(
        r#"{"scripts":{"dev":{"service":[{"path":".","cmd":"echo ${HOME}","env":{"SHELL_DIR":"${HOME}/bin"}}]}}}"#,
    );
}

#[test]
fn branch_template_without_a_worktree_is_not_an_issue() {
    assert_valid(r#"{"scripts":{"dev":{"service":[{"path":".","cmd":"echo ${branch}"}]}}}"#);
}

#[test]
fn cross_instance_port_reference_is_not_flagged_as_unknown() {
    assert_valid(
        r#"{"scripts":{"dev":{"service":[{"path":".","cmd":"true","env":{"API_PORT":"${ports.api@my-repo:main}"}}]}}}"#,
    );
}

#[test]
fn malformed_cross_instance_port_reference_is_an_issue() {
    assert_issue(
        r#"{"scripts":{"dev":{"service":[{"path":".","cmd":"true","env":{"API_PORT":"${ports.api@}"}}]}}}"#,
        "scripts.dev",
        "expected '${ports.<name>@<repo>:<branch>}'",
    );
}

// ---------------------------------------------------------------------------
// Existing configs keep working
// ---------------------------------------------------------------------------

#[test]
fn readme_quick_start_example_is_valid() {
    assert_valid(
        r#"{
          "scripts": {
            "dev": {
              "service": [
                { "name": "web", "path": "/path/to/project", "cmd": "npm run dev" }
              ]
            }
          }
        }"#,
    );
}

#[test]
fn readme_terminal_example_is_valid() {
    assert_valid(
        r#"{
          "scripts": {
            "dev": {
              "terminal": {
                "auth_token": "s3cret",
                "max_sessions_per_ip": 8,
                "max_message_bytes": 65536,
                "idle_timeout_secs": 900
              }
            }
          }
        }"#,
    );
}

#[test]
fn docs_complete_example_is_valid() {
    assert_valid(
        r##"{
          "$schema": "https://raw.githubusercontent.com/Naputt1/fog/main/fog.schema.json",
          "scripts": {
            "dev": {
              "service": [
                {
                  "name": "api",
                  "path": "backend",
                  "cmd": "cargo run",
                  "health_check": {
                    "kind": "tcp",
                    "target": "localhost:8080",
                    "interval_ms": 3000
                  }
                },
                {
                  "path": "frontend",
                  "cmd": "npm run dev"
                }
              ],
              "proxy": {
                "port": 3000,
                "routes": [
                  { "path": "/api", "upstream": "http://localhost:8080" },
                  { "path": "/", "upstream": "http://localhost:5173", "ws": true }
                ]
              }
            }
          },
          "max_scrollback": 5000,
          "sidebar": { "min_width": 14, "max_width": 28 },
          "theme": { "proxy": "#00bcd4", "terminal": "#4caf50" }
        }"##,
    );
}

// ---------------------------------------------------------------------------
// Schema drift
// ---------------------------------------------------------------------------

/// Generates a config value from a JSON-schema node, so every property the
/// schema advertises is exercised without hand-maintaining a parallel fixture.
fn sample_from_schema(schema: &Value, root: &Value) -> Value {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let target = reference
            .strip_prefix("#/")
            .and_then(|path| root.pointer(&format!("/{path}")))
            .unwrap_or_else(|| panic!("unresolvable schema reference {reference}"));
        return sample_from_schema(target, root);
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        // The first branch is the canonical shape (for health checks, the
        // object form; the array form wraps the same object).
        return sample_from_schema(&branches[0], root);
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return values[0].clone();
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let mut object = serde_json::Map::new();
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (key, property) in properties {
                    object.insert(key.clone(), sample_from_schema(property, root));
                }
            } else if let Some(additional) = schema.get("additionalProperties")
                && !additional.is_boolean()
            {
                // `scripts` / `ports`: the key is user chosen, the value is not.
                object.insert("sample".to_string(), sample_from_schema(additional, root));
            }
            Value::Object(object)
        }
        Some("array") => {
            let item = schema
                .get("items")
                .map(|item| sample_from_schema(item, root));
            Value::Array(item.into_iter().collect())
        }
        Some("integer") => {
            let number = schema
                .get("default")
                .and_then(Value::as_i64)
                .or_else(|| schema.get("minimum").and_then(Value::as_i64))
                .unwrap_or(0);
            Value::from(number)
        }
        Some("boolean") => Value::from(
            schema
                .get("default")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        ),
        Some("string") => Value::from(""),
        other => panic!("unsupported schema type {other:?} for {schema}"),
    }
}

#[test]
fn every_schema_property_is_accepted_by_the_config_structs() {
    let schema: Value =
        serde_json::from_str(include_str!("../fog.schema.json")).expect("schema is valid JSON");
    let fixture = sample_from_schema(&schema, &schema);

    // Sanity check the generator: a property the schema advertises but the
    // fixture omits would make this test pass for the wrong reason.
    let schema_script = &schema["properties"]["scripts"]["additionalProperties"];
    let script_properties = schema_script["properties"]
        .as_object()
        .expect("script properties");
    let generated_script = fixture["scripts"]["sample"]
        .as_object()
        .expect("generated script object");
    for key in script_properties.keys() {
        assert!(
            generated_script.contains_key(key),
            "generated script is missing schema property {key}"
        );
    }
    for key in schema["properties"]
        .as_object()
        .expect("top-level properties")
        .keys()
    {
        assert!(
            fixture.get(key).is_some(),
            "generated fixture is missing top-level property {key}"
        );
    }

    let config: Config = serde_json::from_value(fixture.clone()).unwrap_or_else(|error| {
        panic!(
            "schema-only config must deserialize: {error}\n{}",
            serde_json::to_string_pretty(&fixture).expect("fixture is JSON")
        )
    });
    assert_eq!(config.schema.as_deref(), Some(""));
    assert!(config.scripts.contains_key("sample"));
}
