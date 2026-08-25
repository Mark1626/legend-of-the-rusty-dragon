//! Legend of the Pink Dragon, as a pure state machine.
//!
//! The reference implementation is a long-lived process: a self-rescheduling
//! timer drives the world, and output is pushed at an IRC socket as it happens.
//! Neither assumption survives serverless hosting, where nothing outlives a
//! request.
//!
//! So this crate holds no I/O, no clock and no async. A turn is a function:
//!
//! ```text
//! step(&mut GameState, Input, now) -> Out
//! ```
//!
//! Everything that varies — the passage of time, the random stream, who is
//! asking — arrives as an argument or lives in [`GameState`], which makes a
//! turn reproducible from its inputs and lets the hosting layer be a thin shell
//! that loads a row, calls `step`, and writes the row back.

pub mod assets;
pub mod battle;
pub mod bboard;
pub mod config;
pub mod dice;
pub mod event;
pub mod numeric;
pub mod odds;
pub mod out;
pub mod quest;
pub mod rng;
pub mod shop;
pub mod state;
pub mod step;
pub mod user;

pub use battle::{PlayerStrategy, Strategy, Warrior};
pub use config::Pacing;
pub use numeric::Cr;
pub use quest::{Quest, QuestId};
pub use out::{Kind, Line, Out};
pub use bboard::BBoard;
pub use rng::GameRng;
pub use shop::Shop;
pub use state::GameState;
pub use step::{AdminCommand, Input, Turn, start, step};
pub use user::User;

/// The game's version, carried over from the reference's `game.__version__`.
pub const VERSION: &str = "0.5.0-rust";

pub fn version() -> &'static str {
    VERSION
}
