use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use anyhow::Result;

use crate::network::{Network, NetworkExt, SharedNetwork};

type Player = String;

pub struct Game {
    players: HashSet<Player>,
    killed: bool,
    tick_turn: u64,
    network: SharedNetwork,
}

impl Game {
    pub fn new(network: SharedNetwork) -> Self {
        Self {
            players: HashSet::new(),
            killed: false,
            tick_turn: 0,
            network,
        }
    }

    pub async fn run_clock(&mut self) -> Result<()> {
        if self.killed {
            return Ok(());
        }

        if (self.tick_turn % 4 == 1) {}

        self.tick_turn += 1;

        Ok(())
    }
}
