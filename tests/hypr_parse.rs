use osm::hypr;

const CLIENTS: &str = r#"[
 {"address":"0x55a1","pid":1234,"class":"com.mitchellh.ghostty","title":"omarchy:dev",
  "workspace":{"id":3,"name":"3"},"monitor":1,
  "at":[12,38],"size":[3416,1390],"floating":false},
 {"address":"0x55a2","pid":5678,"class":"chromium","title":"x",
  "workspace":{"id":5,"name":"5"},"monitor":1,
  "at":[0,0],"size":[100,100],"floating":true}
]"#;

#[test]
fn parses_clients_including_workspace_and_geometry() {
    let cs = hypr::parse_clients(CLIENTS).unwrap();
    assert_eq!(cs.len(), 2);
    assert_eq!(cs[0].address, "0x55a1");
    assert_eq!(cs[0].pid, 1234);
    assert_eq!(cs[0].workspace_id, 3);
    assert_eq!(
        cs[0].monitor_index, 1,
        "a client names its monitor by index only"
    );
    assert_eq!(cs[0].at, (12, 38));
    assert_eq!(cs[0].size, (3416, 1390));
    assert!(!cs[0].floating);
    assert!(cs[1].floating);
}

#[test]
fn a_reply_that_is_not_an_array_is_an_error_not_an_empty_list() {
    // The whole point. A malformed reply that reads as "no windows" is
    // indistinguishable from a genuinely empty desktop, and overwriting a
    // good layout with an empty one is how the original mapping was lost.
    for bad in ["{}", "null", "\"oops\"", "", "not json"] {
        assert!(
            hypr::parse_clients(bad).is_err(),
            "{bad:?} parsed as a client list"
        );
    }
}

#[test]
fn parses_monitors_with_the_identity_fields_placement_needs() {
    let json = r#"[{"name":"DP-1","description":"Dell U2720Q ABC123","make":"Dell",
      "model":"U2720Q","serial":"ABC123","x":0,"y":0,"width":3840,"height":2160,
      "scale":1.5,"transform":0,"focused":true}]"#;
    let ms = hypr::parse_monitors(json).unwrap();
    assert_eq!(ms.len(), 1);
    assert_eq!(ms[0].name, "DP-1");
    assert_eq!(ms[0].serial, "ABC123");
    assert_eq!(ms[0].scale, 1.5);
    assert!(ms[0].focused);
}

#[test]
fn a_monitor_reply_that_is_not_an_array_is_an_error() {
    for bad in ["{}", "null", ""] {
        assert!(hypr::parse_monitors(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn missing_optional_monitor_identity_still_parses() {
    // Not every compositor build reports make/model/serial.
    let json = r#"[{"name":"eDP-1","description":"","x":0,"y":0,
      "width":1920,"height":1080,"scale":1.0,"transform":0,"focused":true}]"#;
    let ms = hypr::parse_monitors(json).unwrap();
    assert_eq!(ms[0].serial, "");
}

#[test]
fn a_client_s_monitor_index_resolves_to_a_connector() {
    // A client reports `monitor: 1` and nothing else. Without this lookup,
    // capture stores "1" where placement later looks for "HDMI-A-2", matches
    // nothing, and silently drops every window onto the focused output.
    let json = r#"[
      {"id":0,"name":"DP-1","description":"GWD ARZOPA 0000001716554","serial":"0000001716554",
       "x":0,"y":0,"width":1920,"height":1080,"scale":1.0,"transform":0,"focused":false},
      {"id":1,"name":"HDMI-A-2","description":"Dell Inc. AW3423DWF 8CM42S3","serial":"8CM42S3",
       "x":1920,"y":0,"width":3440,"height":1440,"scale":1.0,"transform":0,"focused":true}
    ]"#;
    let ms = hypr::parse_monitors(json).unwrap();
    let cs = hypr::parse_clients(CLIENTS).unwrap();

    let m = hypr::connector_of(cs[0].monitor_index, &ms).expect("index 1 is a real monitor");
    assert_eq!(m.name, "HDMI-A-2");
    assert_eq!(m.serial, "8CM42S3");

    assert!(
        hypr::connector_of(99, &ms).is_none(),
        "an index no monitor claims is None, not a wrong guess"
    );
}
