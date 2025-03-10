use tycho_simulation::models::Token;
use tycho_simulation::protocol::state::ProtocolSim;

pub struct Route {
    tokens: Vec<Token>,
    pools: Vec<Box<dyn ProtocolSim>>,
}
