use super::{
    DeliveryBody, DeliveryIntent, Sender,
    store::{self, Store},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};

pub(super) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS messaging_attributed_results (
    delivery TEXT PRIMARY KEY REFERENCES messaging_deliveries(id),
    source TEXT NOT NULL REFERENCES messaging_deliveries(id)
);
";

impl Store {
    /// Reuse a committed native reply under its original audience. Preserve a
    /// held intent if an otherwise valid destination cannot receive that source.
    pub(super) fn enqueue_attributed_result(
        &self,
        source: &str,
        destination: &str,
        request: &str,
    ) -> Result<String> {
        self.write(|tx| {
            let route = store::read_route(tx, destination)?.context("Unknown messaging destination")?;
            let (source_route, original, sender, event) = tx.query_row(
                "SELECT d.route,d.intent,d.sender,s.event FROM messaging_deliveries d
                 JOIN messaging_effect_sources s ON s.delivery=d.id
                 JOIN messaging_ingress i ON i.connection=d.connection AND i.event=s.event
                 WHERE d.id=?1 AND d.sender IS NOT NULL AND i.stage='done'
                 AND NOT EXISTS(SELECT 1 FROM messaging_fetch_actions f WHERE f.delivery=d.id AND f.released=0)",
                [source],
                |row| Ok((row.get::<_, String>(0)?, store::decode::<DeliveryIntent>(row, 1)?,
                    store::decode::<Sender>(row, 2)?, row.get::<_,String>(3)?)),
            ).optional()?.context("Unknown committed messaging result")?;
            ensure!(matches!(original.body, DeliveryBody::Reply(_)), "Only a committed reply can be published");
            let id = store::key("attributed-messaging-result", &(destination, request))?;
            let approved_audience = source_route == route.id;
            let intent = DeliveryIntent {
                id: id.clone(),
                room: route.room.clone(),
                reply_anchor: if approved_audience { original.reply_anchor } else { None },
                edit_target: None,
                body: original.body,
            };
            store::enqueue(tx, &route, Some(&sender), &intent)?;
            tx.execute(
                "INSERT OR IGNORE INTO messaging_attributed_results(delivery,source) VALUES (?1,?2)",
                params![id, source],
            )?;
            let bound: String = tx.query_row(
                "SELECT source FROM messaging_attributed_results WHERE delivery=?1", [&id], |row| row.get(0),
            )?;
            ensure!(bound == source, "Messaging result request already belongs to another source");
            if approved_audience {
                tx.execute(
                    "INSERT OR IGNORE INTO messaging_effect_sources(delivery,event) VALUES (?1,?2)",
                    params![id,event],
                )?;
                super::profile::link_resource_delivery(tx, &route.connection, &event, &id)?;
            } else {
                tx.execute(
                    "UPDATE messaging_deliveries SET stage='blocked',reason='source_audience_unauthorized'
                     WHERE id=?1 AND stage='pending'",
                    [&id],
                )?;
            }
            Ok(id)
        })
    }
}
