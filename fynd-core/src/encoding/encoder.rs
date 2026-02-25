use tycho_execution::encoding::{
    errors::EncodingError,
    evm::{
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
    },
    models::UserTransferType,
    tycho_encoder::TychoEncoder,
};
use tycho_simulation::tycho_common::{
    models::{protocol::ProtocolComponent, Chain},
    simulation::protocol_sim::ProtocolSim,
};

use crate::{feed::market_data::SharedMarketDataRef, types::OrderSolution, SolveError};

pub struct Encoder {
    tycho_encoder: Box<dyn TychoEncoder>,
    market_data: SharedMarketDataRef,
}

impl Encoder {
    pub fn new(
        chain: Chain,
        transfer_type: UserTransferType,
        swap_encoder_registry: SwapEncoderRegistry,
        market_data: SharedMarketDataRef,
    ) -> Result<Self, SolveError> {
        Ok(Self {
            tycho_encoder: TychoRouterEncoderBuilder::new()
                .chain(chain)
                .user_transfer_type(transfer_type.clone())
                .swap_encoder_registry(swap_encoder_registry)
                .build()?,
            market_data,
        })
    }

    pub async fn encode(
        &self,
        solutions: Vec<OrderSolution>,
    ) -> Result<Vec<OrderSolution>, SolveError> {
        // loop through solutions and convert into the execution Solution model
        //   use the self.market_data to get the ProtocolComponent and ProtocolSim
        // call self.tycho_encoder.encode_solutions()
        // put the EncodedSolution in the corresponding OrderSolution
        // return all the OrderSolutions
        Ok(solutions)
    }

    async fn get_component(&mut self, id: &str) -> Result<ProtocolComponent, SolveError> {
        let market = self.market_data.read().await;
        let component = market
            .get_component(id)
            .cloned()
            .ok_or(SolveError::FailedEncoding("no component found".to_string()))?;
        Ok(component)
    }

    async fn get_simulation_state(&mut self, id: &str) -> Result<Box<dyn ProtocolSim>, SolveError> {
        let market = self.market_data.read().await;
        let state = market
            .get_simulation_state(id)
            .map(|state| state.clone_box())
            .ok_or(SolveError::FailedEncoding("no state found".to_string()))?;
        Ok(state)
    }
}

impl From<EncodingError> for SolveError {
    fn from(err: EncodingError) -> Self {
        SolveError::FailedEncoding(err.to_string())
    }
}
