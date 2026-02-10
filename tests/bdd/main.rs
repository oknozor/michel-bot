#![recursion_limit = "256"]

mod world;
mod steps;

use cucumber::World;

#[tokio::main]
async fn main() {
    world::TestWorld::run("tests/features").await;
}
