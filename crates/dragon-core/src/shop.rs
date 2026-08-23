//! Absalom's store.
//!
//! Stock is a rotating multiset drawn against `cost^gamma`, so cheap gear is
//! common and the good stuff is a windfall.
//!
//! One presentation change from the reference: the IRC build reprinted the
//! whole inventory on every rotation, because a channel has no way to ask what
//! is in stock. A web client just reads [`Shop::stock`], so the feed instead
//! carries the news — what sold out, what arrived.

use serde::{Deserialize, Serialize};

use crate::assets::{self, ItemId, SHOP_GAMMA};
use crate::out::{Line, Out};
use crate::rng::GameRng;

/// How many items the store opens with.
const OPENING_STOCK: usize = 12;

/// The store's rotation runs on a 32-step cycle, restocking on odd steps and
/// twice over at step 17.
const CYCLE: u32 = 32;
const DOUBLE_RESTOCK_STEP: u32 = 17;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shop {
    /// How many of each catalogue item are on the shelves, indexed by item id.
    available: Vec<u32>,
    /// Position in the restock cycle, not a duration.
    tick_cnt: u32,
}

impl Default for Shop {
    fn default() -> Self {
        Self { available: vec![0; assets::items().len()], tick_cnt: 0 }
    }
}

impl Shop {
    /// Open for business with a random opening inventory.
    pub fn new(rng: &mut GameRng) -> Self {
        let mut shop = Shop::default();
        for _ in 0..OPENING_STOCK {
            let item = shop.random_item(rng);
            shop.available[item as usize] += 1;
        }
        shop
    }

    /// Draw one item from the catalogue, weighted so dear things are rare.
    fn random_item(&self, rng: &mut GameRng) -> ItemId {
        let target = rng.unit() * assets::shop_total_weight();
        let mut cumulative = 0.0;
        for item in assets::items() {
            cumulative += (item.cost as f64).powf(SHOP_GAMMA);
            if target < cumulative {
                return item.id;
            }
        }
        // Only reachable through floating-point drift at the very top.
        assets::items().last().expect("catalogue is never empty").id
    }

    /// Everything currently for sale, in catalogue order.
    pub fn stock(&self) -> Vec<(ItemId, u32)> {
        self.available
            .iter()
            .enumerate()
            .filter(|&(_, &count)| count > 0)
            .map(|(id, &count)| (id as ItemId, count))
            .collect()
    }

    pub fn count_of(&self, id: ItemId) -> u32 {
        self.available.get(id as usize).copied().unwrap_or(0)
    }

    pub fn total_items(&self) -> u32 {
        self.available.iter().sum()
    }

    /// Advance the restock cycle.
    pub fn tick(&mut self, rng: &mut GameRng, out: &mut Out) {
        self.tick_cnt = (self.tick_cnt + 1) % CYCLE;
        if self.tick_cnt % 2 == 1 {
            let count = 1 + u32::from(self.tick_cnt == DOUBLE_RESTOCK_STEP);
            self.rotate_stock(count as usize, rng, out);
        }
    }

    /// Sell off `count` items at random and bring in replacements.
    fn rotate_stock(&mut self, count: usize, rng: &mut GameRng, out: &mut Out) {
        // Flatten the multiset so an item held three times is three times as
        // likely to be the one that goes, and can go more than once.
        let shelves: Vec<ItemId> = self
            .available
            .iter()
            .enumerate()
            .flat_map(|(id, &n)| std::iter::repeat_n(id as ItemId, n as usize))
            .collect();
        // The reference would raise here rather than sell what it hasn't got.
        let count = count.min(shelves.len());

        let mut departed = Vec::new();
        let mut arrived = Vec::new();
        for slot in rng.sample_indices(shelves.len(), count) {
            let leaving = shelves[slot];
            self.available[leaving as usize] -= 1;
            departed.push(leaving);

            let coming = self.random_item(rng);
            self.available[coming as usize] += 1;
            arrived.push(coming);
        }

        if arrived.is_empty() {
            return;
        }
        let mut line = Line::store().text(" Absalom restocks: ");
        for (i, &id) in arrived.iter().enumerate() {
            if i > 0 {
                line = line.text(", ");
            }
            let item = assets::item(id).expect("stocked item is in the catalogue");
            line = line
                .bold(item.tag())
                .text(" ")
                .text(&item.name)
                .text(" (cost: ")
                .text(item.cost)
                .text(")");
        }
        let sold: Vec<&str> = departed
            .iter()
            .filter_map(|&id| assets::item(id).map(|i| i.name.as_str()))
            .collect();
        if !sold.is_empty() {
            line = line.text(", clearing out the ").text(sold.join(", "));
        }
        out.broadcast(line.text("."));
    }

    /// Top the shelves back up after a sale.
    fn replenish(&mut self, rng: &mut GameRng) {
        let held = self.total_items();
        let low = if held < 8 { 1 } else { 0 };
        let high = if held > 10 { 1 } else { 2 };
        for _ in 0..rng.range(low, high + 1) {
            let item = self.random_item(rng);
            self.available[item as usize] += 1;
        }
    }

    /// Sell `id` to `user`, if it is in stock and they can afford it.
    ///
    /// The purchase is applied to the user here; the caller only has to hand
    /// over the buyer.
    pub fn sell(
        &mut self,
        user: &mut crate::user::User,
        id: ItemId,
        rng: &mut GameRng,
        out: &mut Out,
    ) {
        let Some(item) = assets::item(id) else {
            out.reply(Line::store().text("There is no such item in the store!"));
            return;
        };

        if self.count_of(id) == 0 {
            out.reply(
                Line::store()
                    .text("There is currently no ")
                    .text(&item.name)
                    .text(" in the store!"),
            );
            return;
        }
        if user.money < item.cost {
            out.reply(
                Line::store()
                    .text("You don't have enough money for buying a ")
                    .text(&item.name)
                    .text(" (cost: ")
                    .text(item.cost)
                    .text(")!"),
            );
            return;
        }
        if !user.buy(id) {
            out.reply(
                Line::store().text("You already have a ").text(&item.name).text("!"),
            );
            return;
        }

        self.available[id as usize] -= 1;
        self.replenish(rng);

        out.broadcast(
            Line::store()
                .text(" ")
                .nick(&user.nick)
                .text(" has bought a ")
                .text(&item.name)
                .text(" (cost: ")
                .text(item.cost)
                .text(")."),
        );
        out.reply(
            Line::store()
                .text("You have bought a ")
                .text(&item.name)
                .text(" (cost: ")
                .text(item.cost)
                .text("). Remaining money = ")
                .text(user.money)
                .text("."),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user::User;

    fn item_named(name: &str) -> ItemId {
        assets::items().iter().find(|i| i.name == name).unwrap().id
    }

    #[test]
    fn a_new_store_opens_with_twelve_items() {
        let mut rng = GameRng::seed_from_u64(1);
        let shop = Shop::new(&mut rng);
        assert_eq!(shop.total_items(), OPENING_STOCK as u32);
        assert!(!shop.stock().is_empty());
        assert!(shop.stock().iter().all(|&(_, n)| n > 0));
    }

    #[test]
    fn stock_only_lists_what_is_actually_held() {
        let mut rng = GameRng::seed_from_u64(2);
        let shop = Shop::new(&mut rng);
        let listed: u32 = shop.stock().iter().map(|&(_, n)| n).sum();
        assert_eq!(listed, shop.total_items());
        for (id, count) in shop.stock() {
            assert_eq!(shop.count_of(id), count);
            assert!(assets::item(id).is_some());
        }
    }

    #[test]
    fn cheap_gear_is_far_more_common_than_expensive_gear() {
        let mut rng = GameRng::seed_from_u64(3);
        let shop = Shop::default();
        let mut cheap = 0;
        let mut dear = 0;
        for _ in 0..20_000 {
            let item = assets::item(shop.random_item(&mut rng)).unwrap();
            if item.cost <= 100 {
                cheap += 1;
            }
            if item.cost >= 100_000 {
                dear += 1;
            }
        }
        assert!(cheap > dear * 5, "cheap {cheap} vs dear {dear}");
        assert!(dear > 0, "expensive items should still turn up sometimes");
    }

    #[test]
    fn the_restock_cycle_only_turns_over_on_odd_steps() {
        let mut rng = GameRng::seed_from_u64(4);
        let mut shop = Shop::new(&mut rng);
        let mut restocks = 0;
        for _ in 0..CYCLE {
            let mut out = Out::new();
            shop.tick(&mut rng, &mut out);
            if !out.feed.is_empty() {
                restocks += 1;
            }
        }
        assert_eq!(restocks, 16, "half of a 32-step cycle");
    }

    #[test]
    fn rotation_keeps_the_shelf_count_steady() {
        let mut rng = GameRng::seed_from_u64(5);
        let mut shop = Shop::new(&mut rng);
        let mut out = Out::new();
        for _ in 0..500 {
            shop.tick(&mut rng, &mut out);
            assert_eq!(
                shop.total_items(),
                OPENING_STOCK as u32,
                "rotation swaps one-for-one"
            );
        }
    }

    #[test]
    fn a_restock_announces_what_arrived() {
        let mut rng = GameRng::seed_from_u64(6);
        let mut shop = Shop::new(&mut rng);
        let mut out = Out::new();
        while out.feed.is_empty() {
            shop.tick(&mut rng, &mut out);
        }
        let said = out.transcript()[0].clone();
        assert!(said.contains("Absalom restocks:"), "{said}");
        assert!(said.contains("cost:"), "{said}");
    }

    #[test]
    fn rotating_an_empty_store_is_harmless() {
        let mut rng = GameRng::seed_from_u64(7);
        let mut shop = Shop::default();
        let mut out = Out::new();
        shop.rotate_stock(3, &mut rng, &mut out);
        assert_eq!(shop.total_items(), 0);
        assert!(out.feed.is_empty());
    }

    #[test]
    fn buying_transfers_the_item_and_the_money() {
        let mut rng = GameRng::seed_from_u64(8);
        let mut shop = Shop::default();
        let club = item_named("Club");
        shop.available[club as usize] = 1;

        let mut user = User::new("Absalom", 0);
        user.money = 500;
        let mut out = Out::new();
        shop.sell(&mut user, club, &mut rng, &mut out);

        assert_eq!(user.weapon, Some(club));
        assert_eq!(user.money, 490);
        assert_eq!(shop.count_of(club), 0, "the last one left the shelf");
        assert!(out.transcript()[0].contains("has bought a Club"));
        assert!(out.reply[0].plain().contains("Remaining money = 490"));
    }

    #[test]
    fn buying_replenishes_the_shelves() {
        let mut rng = GameRng::seed_from_u64(9);
        let mut shop = Shop::new(&mut rng);
        let club = item_named("Club");
        shop.available[club as usize] += 1;
        let before = shop.total_items();

        let mut user = User::new("A", 0);
        user.money = 500;
        let mut out = Out::new();
        shop.sell(&mut user, club, &mut rng, &mut out);
        // One left the shelf, then between zero and two arrived.
        assert!(shop.total_items() >= before - 1);
        assert!(shop.total_items() <= before + 1);
    }

    #[test]
    fn replenishing_pulls_a_thin_store_back_up() {
        let mut rng = GameRng::seed_from_u64(10);
        let mut shop = Shop::default();
        shop.available[0] = 3; // well under the low-water mark of 8
        for _ in 0..20 {
            shop.replenish(&mut rng);
        }
        assert!(shop.total_items() > 3, "a thin store must always gain stock");
    }

    #[test]
    fn replenishing_a_full_store_adds_at_most_one() {
        let mut rng = GameRng::seed_from_u64(11);
        for _ in 0..50 {
            let mut shop = Shop::default();
            shop.available[0] = 11; // above the high-water mark of 10
            shop.replenish(&mut rng);
            assert!(shop.total_items() <= 12);
        }
    }

    #[test]
    fn an_empty_shelf_is_refused_without_charging() {
        let mut rng = GameRng::seed_from_u64(12);
        let mut shop = Shop::default();
        let mut user = User::new("A", 0);
        user.money = 100_000;
        let mut out = Out::new();

        shop.sell(&mut user, item_named("Club"), &mut rng, &mut out);
        assert_eq!(user.money, 100_000);
        assert!(user.weapon.is_none());
        assert!(out.feed.is_empty(), "a refusal is private");
        assert!(out.reply[0].plain().contains("no Club in the store"));
    }

    #[test]
    fn an_unaffordable_item_is_refused_without_charging() {
        let mut rng = GameRng::seed_from_u64(13);
        let mut shop = Shop::default();
        let plate = item_named("Plate");
        shop.available[plate as usize] = 1;

        let mut user = User::new("A", 0);
        user.money = 5;
        let mut out = Out::new();
        shop.sell(&mut user, plate, &mut rng, &mut out);

        assert_eq!(user.money, 5);
        assert!(user.armor.is_none());
        assert_eq!(shop.count_of(plate), 1, "the item stays on the shelf");
        assert!(out.reply[0].plain().contains("don't have enough money"));
    }

    #[test]
    fn buying_a_duplicate_is_refused_and_keeps_the_stock() {
        let mut rng = GameRng::seed_from_u64(14);
        let mut shop = Shop::default();
        let club = item_named("Club");
        shop.available[club as usize] = 2;

        let mut user = User::new("A", 0);
        user.money = 500;
        let mut out = Out::new();
        shop.sell(&mut user, club, &mut rng, &mut out);
        let after_first = user.money;

        let mut out = Out::new();
        shop.sell(&mut user, club, &mut rng, &mut out);
        assert_eq!(user.money, after_first, "the second purchase is free of charge");
        assert!(out.reply[0].plain().contains("already have a Club"));
        assert!(out.feed.is_empty());
    }

    #[test]
    fn an_unknown_item_id_is_reported_politely() {
        let mut rng = GameRng::seed_from_u64(15);
        let mut shop = Shop::default();
        let mut user = User::new("A", 0);
        user.money = 100;
        let mut out = Out::new();
        shop.sell(&mut user, 9_999, &mut rng, &mut out);
        assert!(out.reply[0].plain().contains("no such item"));
        assert_eq!(user.money, 100);
    }

    #[test]
    fn a_shop_survives_a_serde_round_trip() {
        let mut rng = GameRng::seed_from_u64(16);
        let shop = Shop::new(&mut rng);
        let json = serde_json::to_string(&shop).unwrap();
        assert_eq!(serde_json::from_str::<Shop>(&json).unwrap(), shop);
    }
}
