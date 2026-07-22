use std::str::FromStr;

use fynd_core::types::{native_token, parse_chain};
use tycho_simulation::tycho_common::models::{
    chain_config::{init_chain_registry, ChainConfigRegistry},
    Address,
};

const CHAINS_YAML: &str = r#"
chains:
  - name: tempo
    chain_id: 12345
    block_time_secs: 1
    native:
      address: "0x0000000000000000000000000000000000000000"
      symbol: "ETH"
      decimals: 18
    wrapped_native:
      address: "0x4200000000000000000000000000000000000006"
      symbol: "WETH"
      decimals: 18
    default_tvl_thresholds:
      low: 1000
      medium: 10000
"#;

#[test]
fn resolves_and_reads_native_for_custom_chain() {
    init_chain_registry(ChainConfigRegistry::from_yaml_str(CHAINS_YAML).unwrap())
        .expect("registry must be uninitialized in this dedicated test binary");

    // built-in still works
    let eth = parse_chain("Ethereum").unwrap();
    assert!(native_token(&eth).is_ok());

    // custom resolves and exposes its wrapped-native
    let tempo = parse_chain("tempo").expect("custom chain must resolve");
    let wnative = native_token(&tempo).expect("custom wrapped-native must resolve");
    assert_eq!(wnative, Address::from_str("0x4200000000000000000000000000000000000006").unwrap());

    // unknown still rejected
    assert!(parse_chain("not_a_chain").is_err());

    // Starknet's wrapped-native is a placeholder zero address; fail fast rather than
    // returning it as a usable gas token.
    let starknet = parse_chain("starknet").unwrap();
    assert!(native_token(&starknet).is_err());
}
