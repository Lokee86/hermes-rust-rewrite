use super::types::{AccessPolicy, Room, RoomKind, RoomPolicy, Sender};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Owner policy layered from the account to its room and then to the exact thread.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScopePolicy {
    pub denied_users: BTreeSet<String>,
    pub denied_roles: BTreeSet<String>,
    pub direct: RoomRules,
    pub rooms: BTreeMap<String, RoomRules>,
    pub threads: BTreeMap<String, BTreeMap<String, RoomRules>>,
    pub require_actor_evidence: bool,
    pub pair_direct: bool,
    pub events: BTreeMap<String, ResourcePolicy>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoomRules {
    pub enabled: Option<bool>,
    /// An explicit allow set further restricts inherited account grants.
    pub users: Option<BTreeSet<String>>,
    pub roles: Option<BTreeSet<String>>,
    pub denied_users: BTreeSet<String>,
    pub denied_roles: BTreeSet<String>,
    pub mention_required: Option<bool>,
    pub per_participant: Option<bool>,
    pub turn: TurnPolicy,
    pub delivery: Option<DeliveryPolicy>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TurnPolicy {
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub skills: Option<BTreeSet<String>>,
    pub tools: Option<bool>,
    pub approval: Option<ApprovalPolicy>,
    pub approval_reuse: Option<ApprovalReuse>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    Manual,
    Smart,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReuse {
    Session,
    Persistent,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeliveryPolicy {
    pub submitted: bool,
    pub mentions: bool,
    pub notifications: bool,
    pub protection: Option<String>,
}

/// Explicit authorization for non-chat work, scoped to an authenticated
/// operation, resource, individual actor, and returned audience.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResourcePolicy {
    pub actors: BTreeSet<String>,
    pub resources: BTreeSet<String>,
    pub destinations: Vec<Room>,
}

impl AccessPolicy {
    fn rules(&self, room: &Room) -> impl Iterator<Item = &RoomRules> {
        (room.kind == RoomKind::Direct)
            .then_some(&self.scope.direct)
            .into_iter()
            .chain(self.scope.rooms.get(&room.chat))
            .chain(
                room.thread
                    .as_ref()
                    .and_then(|thread| self.scope.threads.get(&room.chat)?.get(thread)),
            )
    }

    pub(super) fn room_policy(&self, room: &Room) -> Option<RoomPolicy> {
        let mut current = match room.kind {
            RoomKind::Direct => RoomPolicy {
                tools: self.direct_tools,
                spoken_replies: self.direct_spoken_replies,
                ..RoomPolicy::default()
            },
            RoomKind::Group => match self.rooms.get(&room.chat) {
                Some(policy) => policy.clone(),
                None if self.rules(room).any(|rules| rules.enabled == Some(true)) => {
                    RoomPolicy::default()
                }
                None => return None,
            },
        };
        for rules in self.rules(room) {
            if rules.enabled == Some(false) {
                return None;
            }
            if let Some(mention) = rules.mention_required {
                current.mention_required = mention;
            }
            if let Some(split) = rules.per_participant {
                current.per_participant = split;
            }
            if let Some(tools) = rules.turn.tools {
                current.tools = tools;
            }
        }
        Some(current)
    }

    pub(super) fn grants_sender(&self, room: &Room, sender: &Sender, paired: bool) -> bool {
        self.room_policy(room).is_some()
            && (!self.scope.denied_users.contains(&sender.id)
                && self.scope.denied_roles.is_disjoint(&sender.roles))
            && (paired || self.allows(sender))
            && self.rules(room).all(|rules| {
                !rules.denied_users.contains(&sender.id)
                    && rules.denied_roles.is_disjoint(&sender.roles)
                    && (rules.users.is_none() && rules.roles.is_none()
                        || rules
                            .users
                            .as_ref()
                            .is_some_and(|users| users.contains(&sender.id))
                        || rules
                            .roles
                            .as_ref()
                            .is_some_and(|roles| !roles.is_disjoint(&sender.roles)))
            })
    }

    pub fn turn_policy(&self, room: &Room) -> TurnPolicy {
        let mut result = TurnPolicy::default();
        for rules in self.rules(room) {
            let next = &rules.turn;
            if next.model.is_some() {
                result.model.clone_from(&next.model);
            }
            if next.prompt.is_some() {
                result.prompt.clone_from(&next.prompt);
            }
            if next.skills.is_some() {
                result.skills.clone_from(&next.skills);
            }
            result.tools = next.tools.or(result.tools);
            result.approval = next.approval.or(result.approval);
            result.approval_reuse = next.approval_reuse.or(result.approval_reuse);
        }
        result
    }

    pub fn delivery_policy(&self, room: &Room) -> DeliveryPolicy {
        self.rules(room)
            .filter_map(|rules| rules.delivery.as_ref())
            .last()
            .cloned()
            .unwrap_or_default()
    }
}
