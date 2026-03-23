use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A SyncOp defines a single change to the task database.
#[derive(PartialEq, Eq, Clone, Debug, Serialize, Deserialize)]
pub(crate) enum SyncOp {
    /// Create a new task.
    ///
    /// On application, if the task already exists, the operation does nothing.
    Create { uuid: Uuid },

    /// Delete an existing task.
    ///
    /// On application, if the task does not exist, the operation does nothing.
    Delete { uuid: Uuid },

    /// Update an existing task, setting the given property to the given value.  If the value is
    /// None, then the corresponding property is deleted.
    ///
    /// If the given task does not exist, the operation does nothing.
    Update {
        uuid: Uuid,
        property: String,
        value: Option<String>,
        timestamp: DateTime<Utc>,
    },
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::errors::Result;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_json_create() -> Result<()> {
        let uuid = Uuid::new_v4();
        let op = SyncOp::Create { uuid };
        let json = serde_json::to_string(&op)?;
        assert_eq!(json, format!(r#"{{"Create":{{"uuid":"{}"}}}}"#, uuid));
        let deser: SyncOp = serde_json::from_str(&json)?;
        assert_eq!(deser, op);
        Ok(())
    }

    #[test]
    fn test_json_delete() -> Result<()> {
        let uuid = Uuid::new_v4();
        let op = SyncOp::Delete { uuid };
        let json = serde_json::to_string(&op)?;
        assert_eq!(json, format!(r#"{{"Delete":{{"uuid":"{}"}}}}"#, uuid));
        let deser: SyncOp = serde_json::from_str(&json)?;
        assert_eq!(deser, op);
        Ok(())
    }

    #[test]
    fn test_json_update() -> Result<()> {
        let uuid = Uuid::new_v4();
        let timestamp = chrono::Utc::now();

        let op = SyncOp::Update {
            uuid,
            property: "abc".into(),
            value: Some("false".into()),
            timestamp,
        };

        let json = serde_json::to_string(&op)?;
        assert_eq!(
            json,
            format!(
                r#"{{"Update":{{"uuid":"{}","property":"abc","value":"false","timestamp":"{:?}"}}}}"#,
                uuid, timestamp,
            )
        );
        let deser: SyncOp = serde_json::from_str(&json)?;
        assert_eq!(deser, op);
        Ok(())
    }

    #[test]
    fn test_json_update_none() -> Result<()> {
        let uuid = Uuid::new_v4();
        let timestamp = chrono::Utc::now();

        let op = SyncOp::Update {
            uuid,
            property: "abc".into(),
            value: None,
            timestamp,
        };

        let json = serde_json::to_string(&op)?;
        assert_eq!(
            json,
            format!(
                r#"{{"Update":{{"uuid":"{}","property":"abc","value":null,"timestamp":"{:?}"}}}}"#,
                uuid, timestamp,
            )
        );
        let deser: SyncOp = serde_json::from_str(&json)?;
        assert_eq!(deser, op);
        Ok(())
    }
}
