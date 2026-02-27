use tycho_execution::encoding::{
    errors::EncodingError,
    evm::{
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
    },
    models::UserTransferType,
    tycho_encoder::TychoEncoder,
};
use tycho_simulation::tycho_common::models::Chain;

use crate::{
    types::{EncodingOptions, OrderSolution},
    SolveError,
};
pub struct Encoder {
    tycho_encoder: Box<dyn TychoEncoder>,
}

impl Encoder {
    pub fn new(
        chain: Chain,
        transfer_type: UserTransferType,
        swap_encoder_registry: SwapEncoderRegistry,
    ) -> Result<Self, SolveError> {
        Ok(Self {
            tycho_encoder: TychoRouterEncoderBuilder::new()
                .chain(chain)
                .user_transfer_type(transfer_type.clone())
                .swap_encoder_registry(swap_encoder_registry)
                .build()?,
        })
    }

    pub async fn encode(
        &self,
        solutions: Vec<OrderSolution>,
        encoding_options: EncodingOptions,
    ) -> Result<Vec<OrderSolution>, SolveError> {
        // loop through solutions and convert into the execution Solution model
        //   use the self.market_data to get the ProtocolComponent and ProtocolSim
        // call self.tycho_encoder.encode_solutions()
        // Encode the full tycho call,
        //   - use signer if it's not None for permit2
        //   - set a meaningful min amount out with the slippage value
        //   - create a Transaction and put it in the OrderSolution
        // return all the OrderSolutions
        Ok(solutions)
    }
}

impl From<EncodingError> for SolveError {
    fn from(err: EncodingError) -> Self {
        SolveError::FailedEncoding(err.to_string())
    }
}
