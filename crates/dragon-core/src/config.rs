//! Pacing.
//!
//! The reference measures almost everything in ticks: a 675-second heartbeat,
//! a purge at 896 ticks, event cooldowns at 16/24/32/64/256 ticks. That is safe
//! only while the tick length never moves. It has moved — a channel you idle in
//! all day and a page you refresh are different games — so every *duration*
//! here is wall-clock seconds and only genuine *cycle positions* stay counters.
//!
//! [`Pacing::IRC`] reproduces the original cadence exactly; [`Pacing::WEB`] is
//! the retune, and is the default.

use serde::{Deserialize, Serialize};

use crate::numeric::Cr;

pub const MINUTE: i64 = 60;
pub const HOUR: i64 = 60 * MINUTE;
pub const DAY: i64 = 24 * HOUR;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pacing {
    /// The world heartbeat. Four ticks make a full cycle: bulletin board,
    /// event, store, event — so events fire twice as often as the other two.
    pub tick_secs: i64,

    /// Idle time after a fight is `idle_base + idle_level_coeff * level^2`,
    /// less a reward for a hard-won victory, plus a penalty for a defeat,
    /// then clamped.
    pub idle_base_secs: i64,
    pub idle_level_coeff: i64,
    pub idle_min_secs: i64,
    pub idle_max_secs: i64,
    /// Subtracted on a win, scaled by the quest's total challenge rating.
    pub idle_victory_cr_relief: i64,
    /// Added on a loss, plus `idle_defeat_per_level` for each level above 1.
    pub idle_defeat_penalty_secs: i64,
    pub idle_defeat_per_level: i64,

    /// A player who has not accepted a quest in this long leaves the Realm.
    pub purge_after_secs: i64,

    /// Per-player event cooldowns, keyed by the reference's own event classes.
    pub chance_cooldown_secs: i64,
    pub potion_cooldown_secs: i64,
    pub glory_cooldown_secs: i64,
    pub party_quest_cooldown_secs: i64,
    pub victim_cooldown_secs: i64,

    /// World-wide event cooldowns.
    pub dragon_sighting_cooldown_secs: i64,
    pub scoreboard_cooldown_secs: i64,

    /// Completing a quest of at least this total CR ends the game for that
    /// player: they ascend and may reincarnate.
    pub ascension_cr: Cr,

    /// The most ticks a single catch-up will simulate. Beyond this the clock
    /// is skipped forward instead, so one unlucky visitor to a long-dormant
    /// Realm never pays to replay days of history.
    pub max_catchup_ticks: u64,
}

impl Pacing {
    /// The original cadence, as played on tilde.chat.
    pub const IRC: Pacing = Pacing {
        tick_secs: 675,

        idle_base_secs: 8 * MINUTE,
        idle_level_coeff: 3,
        idle_min_secs: 5 * MINUTE,
        idle_max_secs: 45 * MINUTE,
        idle_victory_cr_relief: 4,
        idle_defeat_penalty_secs: 120,
        idle_defeat_per_level: 10,

        purge_after_secs: 7 * DAY,

        chance_cooldown_secs: 9 * HOUR,
        potion_cooldown_secs: 6 * HOUR,
        glory_cooldown_secs: 4 * DAY,
        party_quest_cooldown_secs: 12 * HOUR,
        victim_cooldown_secs: 24 * HOUR,

        dragon_sighting_cooldown_secs: 3 * HOUR,
        scoreboard_cooldown_secs: HOUR + 30 * MINUTE,

        ascension_cr: Cr::from_whole(65),
        max_catchup_ticks: 8,
    };

    /// Retuned for a page you refresh rather than a channel you sit in.
    ///
    /// The world beats ~3.75x faster, which is the actual web problem: a board
    /// that only stirs every 45 minutes looks dead to someone who just opened
    /// the page. Rest between fights is only shortened by about a third, which
    /// is what keeps progression meaningful — see `examples/simulate`, where a
    /// shorter rest than this caps a character inside a day and makes ascension
    /// routine. The long social timers (purge, the Path of Glory) stay put,
    /// because they are measured against a player's life rather than the game's
    /// rhythm.
    ///
    /// Measured with 12 bots questing continuously for 14 simulated days:
    /// ~95 quests per player per day, level 20 in ~2 days of unbroken play,
    /// and one ascension across the whole fortnight. The original pacing gives
    /// ~69 quests a day, level 20 in ~4 days, and no ascension at all.
    pub const WEB: Pacing = Pacing {
        tick_secs: 3 * MINUTE,

        idle_base_secs: 5 * MINUTE,
        idle_level_coeff: 2,
        idle_min_secs: 3 * MINUTE,
        idle_max_secs: 25 * MINUTE,
        idle_victory_cr_relief: 2,
        idle_defeat_penalty_secs: 75,
        idle_defeat_per_level: 6,

        purge_after_secs: 7 * DAY,

        chance_cooldown_secs: 2 * HOUR,
        potion_cooldown_secs: 90 * MINUTE,
        glory_cooldown_secs: DAY,
        party_quest_cooldown_secs: 3 * HOUR,
        victim_cooldown_secs: 6 * HOUR,

        dragon_sighting_cooldown_secs: 45 * MINUTE,
        scoreboard_cooldown_secs: 20 * MINUTE,

        ascension_cr: Cr::from_whole(65),
        max_catchup_ticks: 24,
    };

    /// How long a player rests after a fight, in seconds.
    pub fn idle_after_fight(&self, level: i64, total_cr: Cr, victory: bool) -> i64 {
        let mut idle = self.idle_base_secs + self.idle_level_coeff * level * level;
        if victory {
            // The reference relieves `4 * total_cr`; in eighths that is
            // `relief * cr8 / 8`, floored.
            idle -= self.idle_victory_cr_relief * i64::from(total_cr.eighths()) / 8;
        } else {
            idle += self.idle_defeat_penalty_secs + self.idle_defeat_per_level * (level - 1);
        }
        idle.clamp(self.idle_min_secs, self.idle_max_secs)
    }
}

impl Default for Pacing {
    fn default() -> Self {
        Pacing::WEB
    }
}

/// The reference's `Game.time_display`: `2d 03h04m05s`, dropping the day part
/// when there isn't one.
pub fn time_display(seconds: i64) -> String {
    let total = seconds.max(0);
    let (minutes, s) = (total / 60, total % 60);
    let (hours, m) = (minutes / 60, minutes % 60);
    let (days, h) = (hours / 24, hours % 24);
    let prefix = if days > 0 { format!("{days}d ") } else { String::new() };
    format!("{prefix}{h:02}h{m:02}m{s:02}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_display_matches_the_reference_format() {
        assert_eq!(time_display(0), "00h00m00s");
        assert_eq!(time_display(5), "00h00m05s");
        assert_eq!(time_display(65), "00h01m05s");
        assert_eq!(time_display(3661), "01h01m01s");
        assert_eq!(time_display(86_400), "1d 00h00m00s");
        assert_eq!(time_display(90_061), "1d 01h01m01s");
        assert_eq!(time_display(604_800), "7d 00h00m00s");
    }

    #[test]
    fn time_display_never_shows_negative_time() {
        assert_eq!(time_display(-10), "00h00m00s");
    }

    #[test]
    fn irc_tick_counts_reproduce_the_documented_durations() {
        let p = Pacing::IRC;
        // The reference purges at 896 ticks and events run every second tick.
        assert_eq!(896 * p.tick_secs, p.purge_after_secs);
        let event_period = 2 * p.tick_secs;
        assert_eq!(24 * event_period, p.chance_cooldown_secs);
        assert_eq!(16 * event_period, p.potion_cooldown_secs);
        assert_eq!(256 * event_period, p.glory_cooldown_secs);
        assert_eq!(32 * event_period, p.party_quest_cooldown_secs);
        assert_eq!(64 * event_period, p.victim_cooldown_secs);
        assert_eq!(8 * event_period, p.dragon_sighting_cooldown_secs);
        assert_eq!(4 * event_period, p.scoreboard_cooldown_secs);
    }

    #[test]
    fn irc_idle_matches_the_reference_formula() {
        let p = Pacing::IRC;
        // 8min + 3*level^2, clamped to [5min, 45min].
        assert_eq!(p.idle_after_fight(1, Cr::ZERO, true), 8 * 60 + 3);
        assert_eq!(p.idle_after_fight(10, Cr::ZERO, true), 8 * 60 + 300);
        // A hard-won victory shaves 4 seconds per point of CR.
        assert_eq!(
            p.idle_after_fight(10, Cr::from_whole(20), true),
            8 * 60 + 300 - 80
        );
        // A defeat costs 120s plus 10s per level above the first.
        assert_eq!(
            p.idle_after_fight(5, Cr::from_whole(20), false),
            8 * 60 + 75 + 120 + 40
        );
    }

    #[test]
    fn idle_is_always_clamped_into_range() {
        for pacing in [Pacing::IRC, Pacing::WEB] {
            for level in 1..=20 {
                for cr in [Cr::ZERO, Cr::from_whole(30), Cr::from_whole(120)] {
                    for victory in [true, false] {
                        let idle = pacing.idle_after_fight(level, cr, victory);
                        assert!(
                            (pacing.idle_min_secs..=pacing.idle_max_secs).contains(&idle),
                            "level {level} cr {cr} victory {victory} gave {idle}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_web_retune_is_faster_but_keeps_the_long_social_timers() {
        let (irc, web) = (Pacing::IRC, Pacing::WEB);
        assert!(web.tick_secs < irc.tick_secs);
        assert!(web.idle_max_secs < irc.idle_max_secs);
        assert!(web.scoreboard_cooldown_secs < irc.scoreboard_cooldown_secs);
        // Leaving the Realm still takes a real week, and ascension is unchanged.
        assert_eq!(web.purge_after_secs, irc.purge_after_secs);
        assert_eq!(web.ascension_cr, irc.ascension_cr);
    }

    #[test]
    fn the_default_pacing_is_the_web_retune() {
        assert_eq!(Pacing::default(), Pacing::WEB);
    }

    #[test]
    fn pacing_survives_a_serde_round_trip() {
        let json = serde_json::to_string(&Pacing::IRC).unwrap();
        assert_eq!(serde_json::from_str::<Pacing>(&json).unwrap(), Pacing::IRC);
        // Missing fields fall back to the default, so adding knobs later will
        // not strand an in-flight game.
        let partial: Pacing = serde_json::from_str(r#"{"tick_secs":42}"#).unwrap();
        assert_eq!(partial.tick_secs, 42);
        assert_eq!(partial.purge_after_secs, Pacing::WEB.purge_after_secs);
    }
}
