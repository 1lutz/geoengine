use super::database_migration::{DatabaseVersion, Migration};
use crate::error::Result;
use async_trait::async_trait;
use tokio_postgres::Transaction;

/// This migration adds `end_time` to `EdrVectorSpec` and renames `time` to `start_time`
pub struct Migration0009EdrProvider;

#[async_trait]
impl Migration for Migration0009EdrProvider {
    fn prev_version(&self) -> Option<DatabaseVersion> {
        Some("0008_band_names".into())
    }

    fn version(&self) -> DatabaseVersion {
        "0009_edr_provider".into()
    }

    async fn migrate(&self, tx: &Transaction<'_>) -> Result<()> {
        tx.batch_execute(
            r#"
        ALTER TYPE "EdrVectorSpec" 
        RENAME ATTRIBUTE time TO start_time;

        ALTER TYPE "EdrVectorSpec" 
        ADD ATTRIBUTE end_time text;
    "#,
        )
        .await?;

        Ok(())
    }
}
