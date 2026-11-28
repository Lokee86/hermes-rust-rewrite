use super::{
    ApprovalReuse, GatewayError, Ingress, Room, Sender,
    runtime::db,
    store::{self, Action, Route},
    types::Response,
};
use anyhow::Result;
use rusqlite::{Connection, params};

pub(super) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_action_authority (
    id TEXT PRIMARY KEY REFERENCES messaging_actions(id), expires INTEGER NOT NULL, tool TEXT
);
CREATE TABLE IF NOT EXISTS messaging_tool_approvals (
    connection TEXT NOT NULL, route TEXT NOT NULL, actor TEXT NOT NULL,
    session TEXT NOT NULL, tool TEXT NOT NULL, policy TEXT NOT NULL,
    PRIMARY KEY(connection,route,actor,session,tool,policy)
);
";

/// An execution-scoped check against the currently leased route and exact
/// tool request. The session receipt never serializes this capability.
#[derive(Clone)]
pub(crate) struct ToolApprovalContext {
    pub(super) ingress: Ingress,
    pub(super) route: String,
    pub(super) room: Room,
    pub(super) actor: Sender,
    pub(super) session: String,
}

pub(crate) fn tool_key(name: &str, arguments: &str) -> Result<String> {
    store::key("approved-tool", &(name, arguments))
}

impl ToolApprovalContext {
    pub(crate) fn authorized(&self, tool: &str) -> bool {
        self.ingress
            .checked(true, |state| {
                if !self
                    .ingress
                    .authorize_operator(&state.policy, &self.room, &self.actor)?
                    || !state
                        .policy
                        .room_policy(&self.room)
                        .is_some_and(|room| room.tools)
                {
                    return Ok(false);
                }
                let Some(reuse) = state.policy.turn_policy(&self.room).approval_reuse else {
                    return Ok(false);
                };
                let policy = db(store::key(
                    "approval-policy",
                    &(&state.policy, state.revision, self.ingress.installation()),
                ))?;
                self.ingress
                    .store
                    .read(|db| {
                        Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM messaging_tool_approvals WHERE connection = ?1
                 AND route = ?2 AND actor = ?3 AND tool = ?4 AND policy = ?5
                 AND (session = ?6 OR session = '' AND ?7))",
                params![self.ingress.connection,self.route,self.actor.id,tool,policy,
                    self.session,reuse == ApprovalReuse::Persistent],
                |row| row.get(0),
            )?)
                    })
                    .map_err(|_| GatewayError::TemporarilyUnavailable)
            })
            .unwrap_or(false)
    }
}

pub(super) fn permits(response: &Response, scope: Option<ApprovalReuse>) -> bool {
    match response {
        Response::ApproveForSession => scope.is_some(),
        Response::ApprovePersistently => scope == Some(ApprovalReuse::Persistent),
        _ => true,
    }
}

pub(super) fn record(
    db: &Connection,
    route: &Route,
    action: &Action,
    response: &Response,
    policy: &str,
) -> Result<()> {
    let session = match response {
        Response::ApproveForSession => action.session.as_str(),
        Response::ApprovePersistently => "",
        _ => return Ok(()),
    };
    let tool = action
        .tool
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Approval has no exact tool request"))?;
    db.execute(
        "INSERT OR IGNORE INTO messaging_tool_approvals (connection,route,actor,session,tool,policy)
         VALUES (?1,?2,?3,?4,?5,?6)",
        params![route.connection,route.id,action.recipient,session,tool,policy],
    )?;
    Ok(())
}
