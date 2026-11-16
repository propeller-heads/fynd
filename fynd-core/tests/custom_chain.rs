use std::{collections::HashMap, str::FromStr, sync::Once, time::Duration};

use fynd_core::{
    algorithm::test_utils::{token, MockProtocolSim},
    types::{gas_token, parse_chain},
    EncodingOptions, Order, OrderSide, PoolConfig, QuoteOptions, QuoteRequest, QuoteStatus, SolveError,
    Solver,
};
use num_bigint::BigUint;
use tycho_simulation::{
    protocol::models::{ProtocolComponent, Update},
    tycho_client::feed::{BlockHeader, SynchronizerState},
    tycho_common::{
        models::{
            chain_config::{init_chain_registry, ChainConfigRegistry},
            token::Token,
            Address,
        },
        simulation::protocol_sim::ProtocolSim,
        Bytes,
    },
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

fn init_custom_chain() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        init_chain_registry(ChainConfigRegistry::from_yaml_str(CHAINS_YAML).unwrap())
            .expect("registry must be uninitialized in this dedicated test binary");
    });
}

#[test]
fn resolves_and_reads_native_for_custom_chain() {
    init_custom_chain();

    // built-in still works
    let eth = parse_chain("Ethereum").unwrap();
    assert!(gas_token(&eth).is_ok());

    // custom resolves and exposes its wrapped-native
    let tempo = parse_chain("tempo").expect("custom chain must resolve");
    let wnative = gas_token(&tempo).expect("custom wrapped-native must resolve");
    assert_eq!(wnative, Address::from_str("0x4200000000000000000000000000000000000006").unwrap());

    // unknown still rejected
    assert!(parse_chain("not_a_chain").is_err());

    // Starknet's wrapped-native is a placeholder zero address; fail fast rather than
    // returning it as a usable gas token.
    let starknet = parse_chain("starknet").unwrap();
    assert!(gas_token(&starknet).is_err());
}

#[tokio::test]
async fn custom_chain_without_router_builds_and_quotes_but_cannot_encode() {
    init_custom_chain();

    let chain = parse_chain("tempo").expect("custom chain must resolve");
    let wrapped_native = Token {
        address: gas_token(&chain).expect("custom wrapped-native must resolve"),
        decimals: 18,
        chain,
        ..token(0x01, "WETH")
    };
    let other = Token { chain, ..token(0x02, "X") };
    let component_id = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let component = ProtocolComponent::new(
        Bytes::from(component_id.as_str()),
        "uniswap_v2".to_string(),
        "uniswap_v2_pool".to_string(),
        chain,
        vec![wrapped_native.clone(), other.clone()],
        vec![],
        HashMap::new(),
        Bytes::default(),
        chrono::NaiveDateTime::default(),
    );
    let state: Box<dyn ProtocolSim> = Box::new(MockProtocolSim::new(2.0).with_gas(50_000));
    let update = Update::new(
        1,
        HashMap::from([(component_id.clone(), state)]),
        HashMap::from([(component_id, component)]),
    )
    .set_sync_states(HashMap::from([(
        "uniswap_v2".to_string(),
        SynchronizerState::Ready(BlockHeader { number: 1, ..Default::default() }),
    )]));
    let pools = HashMap::from([("most_liquid".to_string(), PoolConfig::new("most_liquid"))]);

    let solver = Solver::from_recording(chain, vec![update], pools, Some(BigUint::from(1u8)))
        .await
        .expect("a custom quote-only chain should build without executor configuration");
    solver
        .wait_until_ready(Duration::from_secs(30))
        .await
        .expect("solver should become ready");
    assert_eq!(solver.router_address(), None);

    let order = Order::new(
        wrapped_native.address.clone(),
        other.address.clone(),
        BigUint::from(1_000_000u64),
        OrderSide::Sell,
        Address::zero(20),
    );
    let quote = solver
        .quote(QuoteRequest::new(vec![order.clone()], QuoteOptions::default()))
        .await
        .expect("quote-only mode should still quote");
    assert_eq!(quote.orders()[0].status(), QuoteStatus::Success);

    let error = solver
        .quote(QuoteRequest::new(
            vec![order],
            QuoteOptions::default().with_encoding_options(EncodingOptions::new(0.005)),
        ))
        .await
        .expect_err("encoding must remain disabled without a deployed router");
    assert!(
        matches!(error, SolveError::EncodingUnavailable(ref message) if message.contains("running quote-only")),
        "unexpected encoding error: {error}"
    );
}
