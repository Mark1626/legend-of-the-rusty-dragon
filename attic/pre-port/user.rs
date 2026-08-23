use std::{collections::HashMap, todo};

use crate::shop::{Armor, Weapon};

struct User {
    nick: String,
    money: u64,
    weapon: HashMap<Weapon, u32>,
    armor: HashMap<Armor, u32>,
    hp: u32,
    xp: u32,
    level: u32,
}

impl User {
    fn new() -> Self {
        todo!()
    }
}
