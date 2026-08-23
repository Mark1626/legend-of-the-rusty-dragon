use std::collections::HashMap;

use crate::network::SharedNetwork;

pub struct Weapon {
    name: String, // TODO: Port basic information from weapons.json
}
pub struct Armor {
    name: String, // TODO: Port basic information from armor.json
}

pub struct Shop {
    network: SharedNetwork,
    weapons: HashMap<Weapon, u32>,
    armors: HashMap<Armor, u32>,
    tick_cnt: u64,
}

impl Shop {
    pub fn new(
        network: SharedNetwork,
        weapons: HashMap<Weapon, u32>,
        armors: HashMap<Armor, u32>,
    ) -> Self {
        Self {
            network,
            weapons,
            armors,
            tick_cnt: 0,
        }
    }

    pub fn tick(&mut self) {
        self.tick_cnt = (self.tick_cnt + 1 & 31);
        if self.tick_cnt & 1 == 0 {
            self.replace_item();
        }
        self.display_items();
    }

    fn replace_item(&self) {
        // Randomly increase the quantity of one weapon and decrease the quantity of one weapon
        // Randomly increase the quantity of one armor and decrease the quantity of one armor
    }

    fn display_items(&self) {
        // Display information about weapons and armor
        let msg = "";
        self.network.broadcast(msg);
    }

    // pub fn buy(&self, user)
}
