use std::process::Command;

fn osm() -> Command {
    Command::new(env!("CARGO_BIN_EXE_osm"))
}

#[test]
fn status_json_reports_protocol_version() {
    let out = osm().args(["status", "--json"]).output().expect("run osm");
    assert!(out.status.success(), "osm status failed: {:?}", out);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is valid JSON");
    assert_eq!(v["protocol_version"], 1);
    assert!(v["engine_version"].is_string());
}
