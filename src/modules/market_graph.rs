#[derive(Clone)]
pub struct MarketGraph {
    max_hops: usize,
    // TODO: decide how the graph should look like
    // vertices, edges, pool states, etc.
}

impl MarketGraph {
    pub fn new(max_hops: usize) -> Self {
        MarketGraph { max_hops }
    }
    pub fn build() {
        println!("Building MarketGraph");
    }
    pub fn insert() {
        println!("Inserting into MarketGraph");
    }
    pub fn delete() {
        println!("Deleting from MarketGraph");
    }
    pub fn update() {
        println!("Updating MarketGraph");
    }
    pub fn get_routes() {
        println!("Getting routes from MarketGraph between two tokens and max_hops");
    }
}
