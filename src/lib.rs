pub mod commands;
pub mod config;
pub mod db;
pub mod matrix;
pub mod seerr;
pub mod seerr_client;
pub mod webhook;

use matrix_sdk::Room;
use sqlx::PgPool;

pub struct AppState {
    pub room: Room,
    pub db: PgPool,
}
