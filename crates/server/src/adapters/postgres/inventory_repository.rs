use async_trait::async_trait;
use bamep_domain::{
    record_inventory_on_change, EndpointId, InventoryRevision, InventoryRevisionId,
    InventorySnapshot,
};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use super::shared::{event_payload, to_backend_err, PgDomainEventType};
use crate::ports::{EndpointUpdateError, InventoryRepository};

pub struct PostgresInventoryRepository {
    pool: PgPool,
}

impl PostgresInventoryRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn row_to_revision(row: &sqlx::postgres::PgRow) -> Result<InventoryRevision, EndpointUpdateError> {
    let endpoint_id: uuid::Uuid = row.try_get("endpoint_id").map_err(to_backend_err)?;
    let revision_id: uuid::Uuid = row.try_get("revision_id").map_err(to_backend_err)?;
    let inventory: serde_json::Value = row.try_get("inventory").map_err(to_backend_err)?;
    let inventory = inventory.as_object().cloned().ok_or_else(|| {
        crate::ports::RepositoryError::Backend(
            "persisted inventory revision is not a JSON object".into(),
        )
    })?;
    Ok(InventoryRevision {
        id: InventoryRevisionId(revision_id),
        endpoint_id: EndpointId(endpoint_id),
        snapshot: InventorySnapshot(inventory),
        recorded_at: row.try_get("recorded_at").map_err(to_backend_err)?,
    })
}

#[async_trait]
impl InventoryRepository for PostgresInventoryRepository {
    async fn record_inventory(
        &self,
        endpoint_id: EndpointId,
        inventory: InventorySnapshot,
        recorded_at: DateTime<Utc>,
    ) -> Result<Option<InventoryRevision>, EndpointUpdateError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;
        let endpoint_exists: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT id FROM endpoints WHERE id = $1 FOR UPDATE")
                .bind(endpoint_id.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(to_backend_err)?;
        if endpoint_exists.is_none() {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(EndpointUpdateError::NotFound(endpoint_id));
        }

        let current_row = sqlx::query(
            "SELECT r.endpoint_id, r.revision_id, r.inventory, r.recorded_at \
             FROM endpoints e JOIN inventory_revisions r \
             ON r.endpoint_id = e.id AND r.revision_id = e.current_inventory_revision_id \
             WHERE e.id = $1",
        )
        .bind(endpoint_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(to_backend_err)?;
        let current = current_row.as_ref().map(row_to_revision).transpose()?;
        let Some(change) =
            record_inventory_on_change(endpoint_id, current.as_ref(), inventory, recorded_at)
        else {
            tx.commit().await.map_err(to_backend_err)?;
            return Ok(None);
        };

        let inventory_json = serde_json::Value::Object(change.revision.snapshot.0.clone());
        sqlx::query(
            "INSERT INTO inventory_revisions (endpoint_id, revision_id, inventory, recorded_at) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(endpoint_id.0)
        .bind(change.revision.id.0)
        .bind(inventory_json)
        .bind(change.revision.recorded_at)
        .execute(&mut *tx)
        .await
        .map_err(to_backend_err)?;

        sqlx::query("UPDATE endpoints SET current_inventory_revision_id = $2 WHERE id = $1")
            .bind(endpoint_id.0)
            .bind(change.revision.id.0)
            .execute(&mut *tx)
            .await
            .map_err(to_backend_err)?;

        let event = &change.event;
        sqlx::query(
            "INSERT INTO domain_events \
             (event_id, event_type, event_version, endpoint_id, occurred_at, payload) \
             VALUES ($1, $2, 1, $3, $4, $5)",
        )
        .bind(event.event_id())
        .bind(PgDomainEventType::from(event))
        .bind(endpoint_id.0)
        .bind(event.occurred_at())
        .bind(event_payload(event))
        .execute(&mut *tx)
        .await
        .map_err(to_backend_err)?;

        tx.commit().await.map_err(to_backend_err)?;
        Ok(Some(change.revision))
    }

    async fn find_current_inventory(
        &self,
        endpoint_id: EndpointId,
    ) -> Result<Option<InventoryRevision>, EndpointUpdateError> {
        let endpoint_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM endpoints WHERE id = $1)")
                .bind(endpoint_id.0)
                .fetch_one(&self.pool)
                .await
                .map_err(to_backend_err)?;
        if !endpoint_exists {
            return Err(EndpointUpdateError::NotFound(endpoint_id));
        }
        let row = sqlx::query(
            "SELECT r.endpoint_id, r.revision_id, r.inventory, r.recorded_at \
             FROM endpoints e JOIN inventory_revisions r \
             ON r.endpoint_id = e.id AND r.revision_id = e.current_inventory_revision_id \
             WHERE e.id = $1",
        )
        .bind(endpoint_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(to_backend_err)?;
        row.as_ref().map(row_to_revision).transpose()
    }
}
