//! Typed inputs and outputs at the player boundary.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub type Tick = u64;
pub type PlayerId = u32;
pub type OrderId = u64;
pub type RoomId = u32;

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub x: u32,
    pub y: u32,
}

impl Position {
    /// Manhattan distance on the open square grid.
    pub fn distance(self, other: Self) -> u64 {
        u64::from(self.x.abs_diff(other.x)) + u64::from(self.y.abs_diff(other.y))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "channel", rename_all = "snake_case", deny_unknown_fields)]
pub enum MessageAddress {
    Public {},
    Direct { recipient: PlayerId },
    Group { recipients: Vec<PlayerId> },
    Room { room_id: RoomId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Move {
        destination: Position,
    },
    UpgradeRange {},
    Repair {},
    GiftHealth {
        target: PlayerId,
        #[schemars(range(min = 1))]
        amount: u64,
    },
    Attack {
        target: PlayerId,
    },
    GiftAp {
        target: PlayerId,
        #[schemars(range(min = 1))]
        amount: u64,
    },
    Message {
        address: MessageAddress,
        text: String,
    },
    Vote {
        target: PlayerId,
    },
    DeclarePeace {
        members: Vec<PlayerId>,
        public: bool,
    },
    RevokePeace {
        members: Vec<PlayerId>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewOrder {
    #[schemars(range(min = 1))]
    pub tick: Tick,
    pub action: Action,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum OrderChange {
    Cancel {
        order_id: OrderId,
    },
    Reschedule {
        order_id: OrderId,
        #[schemars(range(min = 1))]
        tick: Tick,
    },
}

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WakeEvent {
    PublicMessage,
    DirectMessage,
    GroupMessage,
    ApGrant,
    Damage,
    OrderFailure,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WakeRequest {
    #[schemars(range(min = 1))]
    pub after_ticks: Option<Tick>,
    pub events: Vec<WakeEvent>,
}

/// A reply either commits in full or enters the retry flow.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlayerReply {
    pub orders: Vec<NewOrder>,
    pub changes: Vec<OrderChange>,
    pub memory: Option<String>,
    pub wake: WakeRequest,
}

/// Generate the JSON schema from the types used to deserialize replies.
/// The engine also validates references and scheduling against the current match.
pub fn reply_schema() -> schemars::Schema {
    schemars::schema_for!(PlayerReply)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeReason {
    Start,
    Scheduled,
    ApGained,
    DamageTaken,
    Eliminated,
    Revived,
    Event(WakeEvent),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PendingOrder {
    pub id: OrderId,
    pub tick: Tick,
    pub action: Action,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Eliminated,
    JuryOnly,
    InvalidMove,
    Occupied,
    Contested,
    InsufficientAp,
    InsufficientHealth,
    TargetEliminated,
    InvalidTarget,
    OutOfRange,
    NoAgreement,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OrderOutcome {
    Applied,
    Transferred { requested: u64, transferred: u64 },
    Failed { reason: Failure },
    CancelledByElimination,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OrderResult {
    pub id: OrderId,
    pub tick: Tick,
    pub action: Action,
    pub outcome: OrderOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PublicTank {
    pub player: PlayerId,
    pub position: Position,
    pub health: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Room {
    pub id: RoomId,
    pub members: Vec<PlayerId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageChannel {
    Public,
    Direct,
    Group,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Message {
    pub sender: PlayerId,
    pub recipients: Vec<PlayerId>,
    pub tick: Tick,
    pub channel: MessageChannel,
    pub room: Option<RoomId>,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DamageReport {
    pub tick: Tick,
    pub damage: u64,
    pub lethal: bool,
    /// In full disclosure, a player ID appears once per hit.
    pub attackers: Option<Vec<PlayerId>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PeaceDeclaration {
    pub members: Vec<PlayerId>,
    pub public: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PublicEvent {
    ApRound {
        tick: Tick,
    },
    Peace {
        tick: Tick,
        player: PlayerId,
        members: Vec<PlayerId>,
        active: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Tank,
    Juror,
}

/// The player-safe view of the match at the end of a tick.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Observation {
    pub player: PlayerId,
    pub tick: Tick,
    pub role: Role,
    pub wake_reasons: Vec<WakeReason>,
    pub board_size: u32,
    pub board: Vec<PublicTank>,
    pub action_points: u64,
    pub range: u64,
    pub pending_orders: Vec<PendingOrder>,
    pub order_results: Vec<OrderResult>,
    pub damage: Vec<DamageReport>,
    pub jury_bonus_ticks: Vec<Tick>,
    pub own_vote: Option<PlayerId>,
    pub own_peace: Vec<PeaceDeclaration>,
    pub public_events: Vec<PublicEvent>,
    pub messages: Vec<Message>,
    pub rooms: Vec<Room>,
    pub memory: String,
    pub memory_used_bytes: usize,
    pub memory_limit_bytes: usize,
}

/// An adapter sends these three parts on every fresh model call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlayerCall {
    pub rules: String,
    pub starting_prompt: String,
    pub observation: Observation,
    pub validation_error: Option<String>,
}
