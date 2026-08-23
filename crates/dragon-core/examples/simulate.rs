//! Headless playtest, for tuning [`Pacing`].
//!
//! The web retune's numbers were chosen by argument, not evidence. This drives
//! a Realm full of bots through simulated days and reports what the pacing
//! actually feels like: how long a level takes, how often fights are won, how
//! much of the day is spent resting.
//!
//! ```text
//! cargo run --release -p dragon-core --example simulate -- --pacing web --players 12 --days 14
//! ```

use std::collections::BTreeMap;

use dragon_core::assets::{self, ItemKind};
use dragon_core::config::Pacing;
use dragon_core::quest::QuestId;
use dragon_core::rng::GameRng;
use dragon_core::state::GameState;
use dragon_core::step::{Input, step};

const START: i64 = 1_700_000_000;

struct Options {
    pacing: Pacing,
    pacing_name: String,
    players: usize,
    days: i64,
    seed: u64,
    /// How far above their level a bot will reach when choosing a quest.
    ambition: f64,
    verbose: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            pacing: Pacing::WEB,
            pacing_name: "web".into(),
            players: 12,
            days: 14,
            seed: 20_260_823,
            ambition: 1.6,
            verbose: false,
        }
    }
}

fn parse_options() -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--pacing" => {
                let name = value()?;
                options.pacing = match name.as_str() {
                    "web" => Pacing::WEB,
                    "irc" => Pacing::IRC,
                    other => return Err(format!("unknown pacing {other:?}, try web or irc")),
                };
                options.pacing_name = name;
            }
            "--players" => options.players = value()?.parse().map_err(|e| format!("{e}"))?,
            "--days" => options.days = value()?.parse().map_err(|e| format!("{e}"))?,
            "--seed" => options.seed = value()?.parse().map_err(|e| format!("{e}"))?,
            "--ambition" => options.ambition = value()?.parse().map_err(|e| format!("{e}"))?,
            // Overrides, so a candidate retune can be swept without a rebuild.
            "--tick" => options.pacing.tick_secs = value()?.parse().map_err(|e| format!("{e}"))?,
            "--idle-base" => {
                options.pacing.idle_base_secs = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--idle-coeff" => {
                options.pacing.idle_level_coeff = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--idle-min" => {
                options.pacing.idle_min_secs = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--idle-max" => {
                options.pacing.idle_max_secs = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--relief" => {
                options.pacing.idle_victory_cr_relief =
                    value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--verbose" => options.verbose = true,
            "--help" | "-h" => return Err("help".into()),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(options)
}

#[derive(Default)]
struct Stats {
    quests_attempted: u64,
    quests_won: u64,
    purchases: u64,
    ascensions: u64,
    feed_lines: u64,
    ticks_replayed: u64,
    /// First time each player reached a given level.
    level_reached_at: BTreeMap<i32, Vec<i64>>,
    /// Seconds each player spent resting.
    resting_secs: BTreeMap<String, i64>,
}

fn main() {
    let options = match parse_options() {
        Ok(options) => options,
        Err(message) => {
            if message != "help" {
                eprintln!("error: {message}\n");
            }
            eprintln!(
                "usage: simulate [--pacing web|irc] [--players N] [--days N] [--seed N]\n\
                 \x20               [--ambition F] [--verbose]\n\
                 \x20               [--tick S] [--idle-base S] [--idle-coeff N] \
                 [--idle-min S] [--idle-max S] [--relief N]"
            );
            std::process::exit(if message == "help" { 0 } else { 2 });
        }
    };
    run(options);
}

fn run(options: Options) {
    let mut rng = GameRng::seed_from_u64(options.seed ^ 0x5eed);
    let (mut state, _) = GameState::new(START, options.seed, options.pacing.clone());
    let mut stats = Stats::default();

    let nicks: Vec<String> = (0..options.players).map(|i| format!("bot{i:02}")).collect();
    for nick in &nicks {
        step(&mut state, Input::Join { nick: clone(nick) }, START);
    }

    // Bots act on a fine grain so rest timers are respected rather than
    // rounded away; the world itself only advances on its own tick.
    let poll = (options.pacing.idle_min_secs / 3).max(15);
    let finish = START + options.days * 86_400;
    let mut clock = START;
    let mut best_level: BTreeMap<String, i32> = BTreeMap::new();

    while clock < finish {
        clock += poll;
        // Rotate who moves first so the same bot does not always get the pick
        // of the board.
        let mut order: Vec<usize> = (0..nicks.len()).collect();
        rng.shuffle(&mut order);

        for index in order {
            let nick = clone(&nicks[index]);
            let Some(user) = state.users.get(&nick) else {
                // Purged or ascended; walk back in.
                step(&mut state, Input::Join { nick: clone(&nick) }, clock);
                continue;
            };
            if user.is_resting(clock) {
                continue;
            }

            if let Some(item) = shopping_list(&state, &nick) {
                step(&mut state, Input::Buy { nick: clone(&nick), item }, clock);
                stats.purchases += 1;
            }

            let Some(quest) = pick_quest(&state, &nick, options.ambition) else {
                continue;
            };
            let before = state.users.get(&nick).map(|u| (u.quest_win, u.level));
            let turn = step(
                &mut state,
                Input::Quest { nick: clone(&nick), quest, strategy: None },
                clock,
            );
            stats.quests_attempted += 1;
            stats.feed_lines += turn.out.feed.len() as u64;
            stats.ticks_replayed += turn.ticks_replayed;
            if turn.ascension.is_some() {
                stats.ascensions += 1;
            }
            if options.verbose {
                for line in turn.out.transcript() {
                    println!("[{}] {line}", stamp(clock - START));
                }
            }

            if let (Some((wins, _)), Some(user)) = (before, state.users.get(&nick)) {
                if user.quest_win > wins {
                    stats.quests_won += 1;
                }
                let seen = best_level.entry(clone(&nick)).or_insert(1);
                while *seen < user.level {
                    *seen += 1;
                    stats.level_reached_at.entry(*seen).or_default().push(clock - START);
                }
                *stats.resting_secs.entry(clone(&nick)).or_default() +=
                    user.rest_remaining(clock);
            }
        }
    }

    report(&options, &state, &stats, finish - START);
}

fn clone(s: &str) -> String {
    s.to_string()
}

/// The hardest quest a bot will reach for, given its level.
///
/// Total CR is roughly comparable to character level, so a bot takes the
/// toughest posting within `ambition x level` and settles for the easiest on
/// offer when nothing qualifies.
fn pick_quest(state: &GameState, nick: &str, ambition: f64) -> Option<QuestId> {
    let user = state.users.get(nick)?;
    let ceiling = (f64::from(user.level) * ambition * 8.0) as i32;
    let posted = state.bboard.posted();
    posted
        .iter()
        .rfind(|(_, quest)| quest.total_cr().eighths() <= ceiling)
        .or_else(|| posted.first())
        .map(|(id, _)| *id)
}

/// The dearest thing in stock a bot can afford that beats what it is carrying.
fn shopping_list(state: &GameState, nick: &str) -> Option<u16> {
    let user = state.users.get(nick)?;
    let cost_of = |slot: Option<u16>| {
        slot.and_then(assets::item).map_or(0, |item| item.cost)
    };
    let (weapon_cost, armor_cost) = (cost_of(user.weapon), cost_of(user.armor));

    state
        .shop
        .stock()
        .into_iter()
        .filter_map(|(id, _)| assets::item(id))
        // Keep a reserve so a bot is never left unable to replace a weapon.
        .filter(|item| item.cost <= user.money / 2)
        .filter(|item| match item.kind() {
            ItemKind::Weapon => item.cost > weapon_cost,
            ItemKind::Armor => item.cost > armor_cost,
        })
        .max_by_key(|item| item.cost)
        .map(|item| item.id)
}

fn stamp(seconds: i64) -> String {
    format!("{:3}d {:02}h{:02}m", seconds / 86_400, seconds / 3_600 % 24, seconds / 60 % 60)
}

fn median(values: &mut [i64]) -> i64 {
    values.sort_unstable();
    values[values.len() / 2]
}

fn report(options: &Options, state: &GameState, stats: &Stats, span: i64) {
    let pacing = &options.pacing;
    println!("\n=== Legend of the Pink Dragon: {} pacing ===", options.pacing_name);
    println!(
        "{} players, {} days simulated, seed {}, ambition {:.2}",
        options.players, options.days, options.seed, options.ambition
    );
    println!(
        "tick {}s | rest {}-{}s | purge {}d | ascension CR {}",
        pacing.tick_secs,
        pacing.idle_min_secs,
        pacing.idle_max_secs,
        pacing.purge_after_secs / 86_400,
        pacing.ascension_cr
    );

    println!("\n-- activity --");
    let attempts = stats.quests_attempted.max(1);
    println!("quests attempted   {}", stats.quests_attempted);
    println!(
        "win rate           {:.1}%",
        100.0 * stats.quests_won as f64 / attempts as f64
    );
    println!(
        "quests per player per day  {:.1}",
        stats.quests_attempted as f64 / options.players as f64 / options.days as f64
    );
    println!("purchases          {}", stats.purchases);
    println!("ascensions         {}", stats.ascensions);
    println!("world ticks        {}", state.tick_turn);
    println!(
        "feed lines per day {:.0}",
        stats.feed_lines as f64 / options.days as f64
    );

    println!("\n-- progression (median time for a player to first reach a level) --");
    let mut any = false;
    for level in [2, 3, 5, 8, 10, 13, 15, 18, 20] {
        if let Some(times) = stats.level_reached_at.get(&level) {
            let reached = times.len();
            let mut times = times.clone();
            println!(
                "  level {level:<3} {:>12}   ({reached}/{} players)",
                stamp(median(&mut times)),
                options.players
            );
            any = true;
        }
    }
    if !any {
        println!("  nobody levelled at all — the board may be too hard for a newcomer");
    }

    println!("\n-- final standings --");
    for user in state.ranking().iter().take(10) {
        let idle_share = stats
            .resting_secs
            .get(&user.nick)
            .map(|&secs| 100.0 * secs as f64 / span as f64)
            .unwrap_or(0.0);
        println!(
            "  {:<8} level {:>2}  {:>9} XP  {:>10} coins  {}W/{}L  rested ~{:.0}%",
            user.nick, user.level, user.xp, user.money, user.quest_win, user.quest_lost,
            idle_share.min(100.0)
        );
    }
    if state.users.is_empty() {
        println!("  the Realm is empty");
    }

    println!("\n-- board & store --");
    println!("quests posted      {}", state.bboard.len());
    println!("total CR on offer  {}", state.posted_cr());
    println!("items in stock     {}", state.shop.total_items());
    println!();
}
