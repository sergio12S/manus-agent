//! Durable local record of every agent wallet operation and the active budget.

use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use sqlx::Row;
use std::str::FromStr;

use super::budget::{Budget, Category, Exposure, SpentToday};

/// Statuses that may already have moved funds and therefore count against the budget.
const COMMITTED_STATUSES: &str =
    "'submitting','submitted','submission_unknown','processed','confirmed','finalized'";

/// An operation waiting for the human reserves its amount, so other calls cannot
/// spend the same allowance meanwhile. Reservations older than this were interrupted.
const APPROVAL_RESERVATION_MINUTES: i64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: String,
    pub request_id: Option<String>,
    pub kind: String,
    pub status: String,
    pub summary: String,
    pub params: serde_json::Value,
    pub exposure: Option<Exposure>,
    pub decision: Option<serde_json::Value>,
    pub approval: Option<String>,
    pub signature: Option<String>,
    pub slot: Option<i64>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    pub async fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS agent_operations (
                id TEXT PRIMARY KEY,
                request_id TEXT UNIQUE,
                request_hash TEXT,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                summary TEXT NOT NULL,
                params_json TEXT NOT NULL,
                exposure_json TEXT,
                category TEXT,
                sol_out INTEGER NOT NULL DEFAULT 0,
                token_mint TEXT,
                token_out INTEGER NOT NULL DEFAULT 0,
                decision_json TEXT,
                approval TEXT,
                signature TEXT,
                blockhash TEXT,
                slot INTEGER,
                error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS agent_operations_created ON agent_operations(created_at)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS agent_budget (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                budget_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn budget(&self) -> anyhow::Result<Option<Budget>> {
        let row = sqlx::query("SELECT budget_json FROM agent_budget WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| Ok(serde_json::from_str(&row.get::<String, _>("budget_json"))?))
            .transpose()
    }

    pub async fn save_budget(&self, budget: &Budget) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO agent_budget (id, budget_json, updated_at) VALUES (1, ?, ?)
             ON CONFLICT(id) DO UPDATE SET budget_json = excluded.budget_json, updated_at = excluded.updated_at",
        )
        .bind(serde_json::to_string(budget)?)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Existing operation for an idempotency key, or None when the key is new.
    pub async fn find_by_request(
        &self,
        request_id: &str,
    ) -> anyhow::Result<Option<(String, OperationRecord)>> {
        let row = sqlx::query("SELECT * FROM agent_operations WHERE request_id = ?")
            .bind(request_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| {
            (
                row.get::<Option<String>, _>("request_hash")
                    .unwrap_or_default(),
                record_from_row(&row),
            )
        }))
    }

    pub async fn insert(
        &self,
        id: &str,
        request_id: Option<&str>,
        request_hash: &str,
        kind: &str,
        summary: &str,
        params: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let now = now();
        sqlx::query(
            "INSERT INTO agent_operations
                (id, request_id, request_hash, kind, status, summary, params_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, 'prepared', ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(request_id)
        .bind(request_hash)
        .bind(kind)
        .bind(summary)
        .bind(params.to_string())
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_evaluation(
        &self,
        id: &str,
        summary: &str,
        exposure: &Exposure,
        decision: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let (token_mint, token_out) = match &exposure.token_out {
            Some((mint, amount)) => (Some(mint.clone()), *amount as i64),
            None => (None, 0),
        };
        sqlx::query(
            "UPDATE agent_operations SET summary = ?, exposure_json = ?, category = ?, sol_out = ?,
                token_mint = ?, token_out = ?, decision_json = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(summary)
        .bind(serde_json::to_string(exposure)?)
        .bind(category_name(exposure.category))
        .bind(exposure.sol_out as i64)
        .bind(token_mint)
        .bind(token_out)
        .bind(decision.to_string())
        .bind(now())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_status(
        &self,
        id: &str,
        status: &str,
        signature: Option<&str>,
        slot: Option<i64>,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE agent_operations SET status = ?, signature = COALESCE(?, signature),
                slot = COALESCE(?, slot), error = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(status)
        .bind(signature)
        .bind(slot)
        .bind(error)
        .bind(now())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record the signature and blockhash durably before the bytes are broadcast.
    pub async fn set_submitting(
        &self,
        id: &str,
        signature: &str,
        blockhash: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE agent_operations SET status = 'submitting', signature = ?, blockhash = ?,
                error = NULL, updated_at = ?
             WHERE id = ?",
        )
        .bind(signature)
        .bind(blockhash)
        .bind(now())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn blockhash(&self, id: &str) -> anyhow::Result<Option<String>> {
        let row = sqlx::query("SELECT blockhash FROM agent_operations WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|row| row.get::<Option<String>, _>("blockhash")))
    }

    /// Whether an operation already owns this transaction signature.
    pub async fn signature_in_use(
        &self,
        signature: &str,
        except_id: Option<&str>,
    ) -> anyhow::Result<bool> {
        let row =
            sqlx::query("SELECT 1 FROM agent_operations WHERE signature = ? AND id != ? LIMIT 1")
                .bind(signature)
                .bind(except_id.unwrap_or(""))
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.is_some())
    }

    pub async fn set_approval(&self, id: &str, approval: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE agent_operations SET approval = ?, updated_at = ? WHERE id = ?")
            .bind(approval)
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<OperationRecord>> {
        let row = sqlx::query("SELECT * FROM agent_operations WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(record_from_row))
    }

    pub async fn recent(&self, limit: i64) -> anyhow::Result<Vec<OperationRecord>> {
        let rows = sqlx::query("SELECT * FROM agent_operations ORDER BY created_at DESC LIMIT ?")
            .bind(limit.clamp(1, 200))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(record_from_row).collect())
    }

    /// Operations whose on-chain outcome is not yet final.
    pub async fn unsettled(&self) -> anyhow::Result<Vec<OperationRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM agent_operations
             WHERE status IN ('submitted','submission_unknown','processed','confirmed')
               AND signature IS NOT NULL
             ORDER BY created_at ASC LIMIT 50",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(record_from_row).collect())
    }

    /// Close reservations whose approval wait was interrupted (crash, restart).
    pub async fn expire_stale_approvals(&self) -> anyhow::Result<u64> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(APPROVAL_RESERVATION_MINUTES))
            .to_rfc3339();
        let result = sqlx::query(
            "UPDATE agent_operations SET status = 'denied', error = 'approval was interrupted',
                updated_at = ?
             WHERE status = 'awaiting_approval' AND updated_at < ?",
        )
        .bind(now())
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Committed and reserved amounts in the trailing 24 hours, excluding `except_id`.
    pub async fn spent_last_day(&self, except_id: Option<&str>) -> anyhow::Result<SpentToday> {
        self.expire_stale_approvals().await?;
        let since = (chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339();
        let rows = sqlx::query(&format!(
            "SELECT category, sol_out, token_mint, token_out FROM agent_operations
             WHERE created_at >= ? AND id != ?
               AND (status IN ({COMMITTED_STATUSES}) OR status = 'awaiting_approval')"
        ))
        .bind(since)
        .bind(except_id.unwrap_or(""))
        .fetch_all(&self.pool)
        .await?;
        let mut spent = SpentToday::default();
        for row in rows {
            let sol_out = row.get::<i64, _>("sol_out").max(0) as u64;
            match row.get::<Option<String>, _>("category").as_deref() {
                Some("convert") => spent.convert_sol = spent.convert_sol.saturating_add(sol_out),
                _ => spent.send_sol = spent.send_sol.saturating_add(sol_out),
            }
            if let Some(mint) = row.get::<Option<String>, _>("token_mint") {
                let amount = row.get::<i64, _>("token_out").max(0) as u64;
                let entry = spent.send_tokens.entry(mint).or_insert(0);
                *entry = entry.saturating_add(amount);
            }
        }
        Ok(spent)
    }
}

fn category_name(category: Category) -> &'static str {
    match category {
        Category::Send => "send",
        Category::Convert => "convert",
    }
}

fn record_from_row(row: &sqlx::sqlite::SqliteRow) -> OperationRecord {
    fn parse<T: serde::de::DeserializeOwned>(
        row: &sqlx::sqlite::SqliteRow,
        column: &str,
    ) -> Option<T> {
        row.get::<Option<String>, _>(column)
            .and_then(|text| serde_json::from_str(&text).ok())
    }
    OperationRecord {
        id: row.get("id"),
        request_id: row.get("request_id"),
        kind: row.get("kind"),
        status: row.get("status"),
        summary: row.get("summary"),
        params: parse(row, "params_json").unwrap_or(serde_json::Value::Null),
        exposure: parse(row, "exposure_json"),
        decision: parse(row, "decision_json"),
        approval: row.get("approval"),
        signature: row.get("signature"),
        slot: row.get("slot"),
        error: row.get("error"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_store() -> (Store, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("manus-agent-{}.db", uuid::Uuid::new_v4()));
        (Store::open(&path).await.unwrap(), path)
    }

    #[tokio::test]
    async fn only_committed_operations_count_against_budget() {
        let (store, path) = temp_store().await;
        let exposure = Exposure {
            category: Category::Send,
            sol_out: 700,
            fees: 5_000,
            token_out: Some(("Mint".into(), 5)),
            recipient: None,
        };
        let decision = serde_json::json!({"decision": "auto"});
        for (id, status) in [
            ("a", "finalized"),
            ("b", "failed"),
            ("c", "submission_unknown"),
        ] {
            store
                .insert(id, None, "h", "send", "s", &serde_json::json!({}))
                .await
                .unwrap();
            store
                .set_evaluation(id, "s", &exposure, &decision)
                .await
                .unwrap();
            store
                .set_status(id, status, None, None, None)
                .await
                .unwrap();
        }
        let spent = store.spent_last_day(None).await.unwrap();
        assert_eq!(spent.send_sol, 1_400);
        assert_eq!(spent.send_tokens.get("Mint"), Some(&10));
        let spent = store.spent_last_day(Some("a")).await.unwrap();
        assert_eq!(spent.send_sol, 700);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn pending_approvals_reserve_until_interrupted() {
        let (store, path) = temp_store().await;
        let exposure = Exposure {
            category: Category::Send,
            sol_out: 900,
            fees: 5_000,
            token_out: None,
            recipient: None,
        };
        store
            .insert("wait", None, "h", "send", "s", &serde_json::json!({}))
            .await
            .unwrap();
        store
            .set_evaluation("wait", "s", &exposure, &serde_json::json!({}))
            .await
            .unwrap();
        store
            .set_status("wait", "awaiting_approval", None, None, None)
            .await
            .unwrap();
        assert_eq!(store.spent_last_day(None).await.unwrap().send_sol, 900);

        // Pretend the wait began long ago, as after a crash.
        sqlx::query("UPDATE agent_operations SET updated_at = '2000-01-01T00:00:00+00:00'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.spent_last_day(None).await.unwrap().send_sol, 0);
        assert_eq!(store.get("wait").await.unwrap().unwrap().status, "denied");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn budget_round_trips() {
        let (store, path) = temp_store().await;
        assert!(store.budget().await.unwrap().is_none());
        let budget = Budget::default_for_cluster("devnet");
        store.save_budget(&budget).await.unwrap();
        assert_eq!(store.budget().await.unwrap(), Some(budget));
        let _ = std::fs::remove_file(path);
    }
}
