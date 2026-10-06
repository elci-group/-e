// SPDX-License-Identifier: MIT
//! End-to-end behaviour of the `colony` binary.
use std::path::PathBuf;
use std::process::{Command, Output};

const EXAMPLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/request.json");

fn colony(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_colony"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

fn json(o: &Output) -> serde_json::Value {
    serde_json::from_slice(&o.stdout).unwrap()
}

/// Writes `contents` to a per-process temp file that is removed on drop.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str, contents: &str) -> Self {
        let path = std::env::temp_dir().join(format!("colony-cli-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        Self(path)
    }
    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn edited(name: &str, edit: impl FnOnce(&mut serde_json::Value)) -> Temp {
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(EXAMPLE).unwrap()).unwrap();
    edit(&mut value);
    Temp::new(name, &value.to_string())
}

#[test]
fn help_and_version() {
    for args in [&[][..], &["--help"], &["-h"]] {
        let o = colony(args);
        assert!(o.status.success());
        assert!(stdout(&o).starts_with(&format!(
            "Colony (:e) {}\n\nUsage: colony <plan|validate|simulate>",
            env!("CARGO_PKG_VERSION")
        )));
    }
    let o = colony(&["--version"]);
    assert!(o.status.success());
    assert_eq!(
        stdout(&o),
        format!("colony {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn validate_example() {
    let o = colony(&["validate", EXAMPLE]);
    assert!(o.status.success());
    assert_eq!(
        json(&o),
        serde_json::json!({"valid": true, "work_units": 4})
    );
}

#[test]
fn plan_example() {
    let o = colony(&["plan", EXAMPLE]);
    assert!(o.status.success());
    let v = json(&o);
    assert_eq!(v["mode"], "plan_only");
    assert_eq!(v["assignments"].as_array().unwrap().len(), 4);
    assert_eq!(v["plan"]["critical_path_ms"], 2000);
    assert_eq!(v["plan"]["swarms"].as_array().unwrap().len(), 4);
}

#[test]
fn simulate_example() {
    let o = colony(&["simulate", EXAMPLE]);
    assert!(o.status.success());
    let v = json(&o);
    assert_eq!(v["mode"], "simulation_complete");
    assert_eq!(v["snapshot"]["completed"], true);
    assert!(v["snapshot"]["states"]
        .as_object()
        .unwrap()
        .values()
        .all(|s| s == "validated"));
    assert_eq!(v["evidence"].as_object().unwrap().len(), 4);
}

#[test]
fn output_is_deterministic() {
    for verb in ["plan", "simulate"] {
        assert_eq!(
            colony(&[verb, EXAMPLE]).stdout,
            colony(&[verb, EXAMPLE]).stdout
        );
    }
}

fn fails(args: &[&str], stderr_contains: &str) {
    let o = colony(args);
    assert_eq!(o.status.code(), Some(1), "{args:?}");
    assert!(o.stdout.is_empty(), "{args:?}");
    let stderr = String::from_utf8(o.stderr).unwrap();
    assert!(stderr.starts_with("colony: "), "{stderr}");
    assert!(stderr.contains(stderr_contains), "{stderr}");
}

#[test]
fn usage_errors_exit_one() {
    fails(&["deploy", EXAMPLE], "expected plan, validate or simulate");
    fails(&["plan"], "expected plan, validate or simulate");
    fails(
        &["plan", EXAMPLE, "extra"],
        "expected plan, validate or simulate",
    );
    fails(&["--version", "x"], "expected plan, validate or simulate");
}

#[test]
fn input_errors_exit_one() {
    fails(&["validate", "/nonexistent/colony.json"], "No such file");
    let malformed = Temp::new("malformed.json", "{ not json");
    fails(&["validate", malformed.path()], "key must be a string");
    let unknown = edited("unknown.json", |v| v["surprise"] = true.into());
    fails(&["validate", unknown.path()], "unknown field `surprise`");
}

#[test]
fn contract_errors_exit_one() {
    let cyclic = edited("cyclic.json", |v| {
        v["graph"]["nodes"][0]["dependencies"] = serde_json::json!(["synthesis"]);
    });
    fails(
        &["validate", cyclic.path()],
        "InvalidGraph: dependency cycle",
    );
    let starved = edited("starved.json", |v| {
        for r in v["registry"]["resources"].as_array_mut().unwrap() {
            r["remaining_calls"] = 1.into();
        }
    });
    fails(
        &["plan", starved.path()],
        "ProviderUnavailable: no eligible provider",
    );
}
