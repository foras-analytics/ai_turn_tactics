//! Match setup and deterministic spawn placement.

use rand::{SeedableRng, rngs::StdRng, seq::SliceRandom};
use serde::{Deserialize, Serialize};

use crate::{EngineError, Position, Tick};

pub(crate) const START_RANGE: u64 = 2;
pub(crate) const START_HEALTH: u64 = 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnMode {
    EvenlySpaced,
    #[default]
    RandomSeparated,
    Random,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttackerDisclosure {
    Full,
    #[default]
    LethalOnly,
    Hidden,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerConfig {
    pub starting_prompt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    pub players: Vec<PlayerConfig>,
    pub seed: u64,
    pub spawn_mode: SpawnMode,
    pub attacker_disclosure: AttackerDisclosure,
    pub ap_interval: Tick,
    pub ap_round_limit: u64,
    pub memory_limit_bytes: usize,
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            players: vec![PlayerConfig::default(); 6],
            seed: 0,
            spawn_mode: SpawnMode::default(),
            attacker_disclosure: AttackerDisclosure::default(),
            ap_interval: 100,
            ap_round_limit: 30,
            memory_limit_bytes: 32 * 1024,
        }
    }
}

impl MatchConfig {
    pub(crate) fn validate(&self) -> Result<Tick, EngineError> {
        if self.players.len() < 5 || u32::try_from(self.players.len()).is_err() {
            return Err(EngineError::InvalidConfig(
                "a match needs at least five players and valid player IDs",
            ));
        }
        if self.ap_interval == 0 || self.ap_round_limit == 0 || self.memory_limit_bytes == 0 {
            return Err(EngineError::InvalidConfig(
                "AP timing and the memory limit must be positive",
            ));
        }
        // Gifts and loot conserve resources. Bound the total amount that can ever exist.
        let player_count = self.players.len() as u64;
        if player_count
            .checked_mul(self.ap_round_limit)
            .and_then(|ap| ap.checked_add(player_count * START_HEALTH))
            .is_none()
        {
            return Err(EngineError::InvalidConfig(
                "the round limit exceeds the resource counter capacity",
            ));
        }
        self.ap_interval
            .checked_mul(self.ap_round_limit)
            .ok_or(EngineError::InvalidConfig(
                "the round limit exceeds the tick counter capacity",
            ))
    }
}

pub(crate) fn make_board(config: &MatchConfig) -> (u32, Vec<Position>) {
    let player_count = config.players.len();
    let area = 15 * player_count as u64;
    let root = area.isqrt();
    let side = (root + u64::from(root * root < area)) as u32;
    let mut cells: Vec<_> = (0..side)
        .flat_map(|y| (0..side).map(move |x| Position { x, y }))
        .collect();
    let mut rng = StdRng::seed_from_u64(config.seed);
    cells.shuffle(&mut rng);
    let starts = match config.spawn_mode {
        SpawnMode::Random => cells.into_iter().take(player_count).collect(),
        SpawnMode::RandomSeparated => {
            let mut selected = Vec::<Position>::new();
            for cell in cells {
                if selected
                    .iter()
                    .all(|start| start.distance(cell) > START_RANGE)
                {
                    selected.push(cell);
                    if selected.len() == player_count {
                        break;
                    }
                }
            }
            // Each start excludes at most 13 cells. The board has at least 15 per tank.
            debug_assert_eq!(selected.len(), player_count);
            selected
        }
        SpawnMode::EvenlySpaced => evenly_spaced(cells, player_count),
    };
    (side, starts)
}

fn evenly_spaced(mut cells: Vec<Position>, count: usize) -> Vec<Position> {
    // Maximize distance to the nearest chosen start. The shuffled cell order breaks ties.
    let mut selected = vec![cells.pop().expect("board has cells")];
    let mut nearest = vec![u64::MAX; cells.len()];
    while selected.len() < count {
        let last = *selected.last().expect("a start has been chosen");
        for (distance, cell) in nearest.iter_mut().zip(&cells) {
            *distance = (*distance).min(last.distance(*cell));
        }
        let (index, _) = nearest
            .iter()
            .enumerate()
            .max_by_key(|(_, distance)| *distance)
            .expect("board has more cells than tanks");
        selected.push(cells.swap_remove(index));
        nearest.swap_remove(index);
    }
    selected
}
