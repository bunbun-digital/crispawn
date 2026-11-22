// SPDX-License-Identifier: Apache-2.0
use crispawn_engine::{
    connection::{Local, Readiness},
    live_config,
    network_config::Network,
};
use serde_json::json;
#[test]
fn community_rejects_all_network_settings_and_has_no_network_capabilities() {
    assert!(Local.poll().unwrap());
    Network::None {}.validate().unwrap();
    for value in [
        json!({"mode":"auto","wifi_profiles":[]}),
        json!({"mode":"ethernet","ipv4":{"method":"dhcp"}}),
        json!({"mode":"wifi","country":"JP","ipv4":{"method":"dhcp"},
                         "wifi_profile":{"ssid":"test","security":"open"}}),
    ] {
        let network: Network = serde_json::from_value(value).unwrap();
        assert_eq!(
            network.validate().unwrap_err().code,
            "UNSUPPORTED_CAPABILITY"
        );
    }
    assert!(!live_config::Settings::capability_names()
        .iter()
        .any(|v| v.starts_with("network.") || v == "destination.smb" || v == "destination.s3"));
    assert!(serde_json::from_value::<Network>(json!({"mode":"none","unexpected":1})).is_err());
}
