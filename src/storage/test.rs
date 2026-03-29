//! Tests for storage backends. This tests consistency across multiple method calls, to ensure that
//! all implementations are consistent.

use super::{Storage, TaskMap};
use crate::errors::Result;
use crate::storage::taskmap_with;
use crate::Operation;
use chrono::Utc;
use pretty_assertions::assert_eq;
use uuid::Uuid;

/// Subset of storage tests for backends that don't use TC's sync protocol
/// (e.g. PowerSync, where an external daemon handles sync).
macro_rules! storage_tests_no_sync {
    ($storage:expr) => {
        #[tokio::test]
        async fn drop_transaction() -> $crate::errors::Result<()> {
            $crate::storage::test::drop_transaction($storage).await
        }

        #[tokio::test]
        async fn create() -> $crate::errors::Result<()> {
            $crate::storage::test::create($storage).await
        }

        #[tokio::test]
        async fn create_exists() -> $crate::errors::Result<()> {
            $crate::storage::test::create_exists($storage).await
        }

        #[tokio::test]
        async fn get_missing() -> $crate::errors::Result<()> {
            $crate::storage::test::get_missing($storage).await
        }

        #[tokio::test]
        async fn set_task() -> $crate::errors::Result<()> {
            $crate::storage::test::set_task($storage).await
        }

        #[tokio::test]
        async fn delete_task_missing() -> $crate::errors::Result<()> {
            $crate::storage::test::delete_task_missing($storage).await
        }

        #[tokio::test]
        async fn delete_task_exists() -> $crate::errors::Result<()> {
            $crate::storage::test::delete_task_exists($storage).await
        }

        #[tokio::test]
        async fn all_tasks_empty() -> $crate::errors::Result<()> {
            $crate::storage::test::all_tasks_empty($storage).await
        }

        #[tokio::test]
        async fn all_tasks_and_uuids() -> $crate::errors::Result<()> {
            $crate::storage::test::all_tasks_and_uuids($storage).await
        }

        #[tokio::test]
        async fn task_operations() -> $crate::errors::Result<()> {
            $crate::storage::test::task_operations($storage).await
        }

        #[tokio::test]
        async fn tag_metadata_round_trip() -> $crate::errors::Result<()> {
            $crate::storage::test::tag_metadata_round_trip($storage).await
        }

        #[tokio::test]
        async fn tag_metadata_update() -> $crate::errors::Result<()> {
            $crate::storage::test::tag_metadata_update($storage).await
        }

        #[tokio::test]
        async fn get_all_tags() -> $crate::errors::Result<()> {
            $crate::storage::test::get_all_tags($storage).await
        }
    };
}
pub(crate) use storage_tests_no_sync;

pub(super) async fn drop_transaction(mut storage: impl Storage) -> Result<()> {
    let uuid1 = Uuid::new_v4();
    let uuid2 = Uuid::new_v4();

    {
        let mut txn = storage.txn().await?;
        assert!(txn.create_task(uuid1).await?);
        txn.commit().await?;
    }

    {
        let mut txn = storage.txn().await?;
        assert!(txn.create_task(uuid2).await?);
        std::mem::drop(txn); // Unnecessary explicit drop of transaction
    }

    {
        let mut txn = storage.txn().await?;
        let uuids = txn.all_task_uuids().await?;

        assert_eq!(uuids, [uuid1]);
    }

    Ok(())
}

pub(super) async fn create(mut storage: impl Storage) -> Result<()> {
    let uuid = Uuid::new_v4();
    {
        let mut txn = storage.txn().await?;
        assert!(txn.create_task(uuid).await?);
        txn.commit().await?;
    }
    {
        let mut txn = storage.txn().await?;
        let task = txn.get_task(uuid).await?;
        assert_eq!(task, Some(taskmap_with(vec![])));
    }
    Ok(())
}

pub(super) async fn create_exists(mut storage: impl Storage) -> Result<()> {
    let uuid = Uuid::new_v4();
    {
        let mut txn = storage.txn().await?;
        assert!(txn.create_task(uuid).await?);
        txn.commit().await?;
    }
    {
        let mut txn = storage.txn().await?;
        assert!(!txn.create_task(uuid).await?);
        txn.commit().await?;
    }
    Ok(())
}

pub(super) async fn get_missing(mut storage: impl Storage) -> Result<()> {
    let uuid = Uuid::new_v4();
    {
        let mut txn = storage.txn().await?;
        let task = txn.get_task(uuid).await?;
        assert_eq!(task, None);
    }
    Ok(())
}

pub(super) async fn set_task(mut storage: impl Storage) -> Result<()> {
    let uuid = Uuid::new_v4();
    {
        let mut txn = storage.txn().await?;
        txn.set_task(uuid, taskmap_with(vec![("k".to_string(), "v".to_string())]))
            .await?;
        txn.commit().await?;
    }
    {
        let mut txn = storage.txn().await?;
        let task = txn.get_task(uuid).await?;
        assert_eq!(
            task,
            Some(taskmap_with(vec![("k".to_string(), "v".to_string())]))
        );
    }
    Ok(())
}

pub(super) async fn delete_task_missing(mut storage: impl Storage) -> Result<()> {
    let uuid = Uuid::new_v4();
    {
        let mut txn = storage.txn().await?;
        assert!(!txn.delete_task(uuid).await?);
    }
    Ok(())
}

pub(super) async fn delete_task_exists(mut storage: impl Storage) -> Result<()> {
    let uuid = Uuid::new_v4();
    {
        let mut txn = storage.txn().await?;
        assert!(txn.create_task(uuid).await?);
        txn.commit().await?;
    }
    {
        let mut txn = storage.txn().await?;
        assert!(txn.delete_task(uuid).await?);
    }
    Ok(())
}

pub(super) async fn all_tasks_empty(mut storage: impl Storage) -> Result<()> {
    {
        let mut txn = storage.txn().await?;
        let tasks = txn.all_tasks().await?;
        assert_eq!(tasks, vec![]);
    }
    Ok(())
}

pub(super) async fn all_tasks_and_uuids(mut storage: impl Storage) -> Result<()> {
    let uuid1 = Uuid::new_v4();
    let uuid2 = Uuid::new_v4();
    {
        let mut txn = storage.txn().await?;
        assert!(txn.create_task(uuid1).await?);
        txn.set_task(
            uuid1,
            taskmap_with(vec![("num".to_string(), "1".to_string())]),
        )
        .await?;
        assert!(txn.create_task(uuid2).await?);
        txn.set_task(
            uuid2,
            taskmap_with(vec![("num".to_string(), "2".to_string())]),
        )
        .await?;
        txn.commit().await?;
    }
    {
        let mut txn = storage.txn().await?;
        let mut tasks = txn.all_tasks().await?;

        // order is nondeterministic, so sort by uuid
        tasks.sort_by(|a, b| a.0.cmp(&b.0));

        let mut exp = vec![
            (
                uuid1,
                taskmap_with(vec![("num".to_string(), "1".to_string())]),
            ),
            (
                uuid2,
                taskmap_with(vec![("num".to_string(), "2".to_string())]),
            ),
        ];
        exp.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(tasks, exp);
    }
    {
        let mut txn = storage.txn().await?;
        let mut uuids = txn.all_task_uuids().await?;
        uuids.sort();

        let mut exp = vec![uuid1, uuid2];
        exp.sort();

        assert_eq!(uuids, exp);
    }
    Ok(())
}

pub(super) async fn tag_metadata_round_trip(mut storage: impl Storage) -> Result<()> {
    {
        let mut txn = storage.txn().await?;
        // No metadata set yet.
        assert_eq!(txn.get_tag_metadata("work".into()).await?, None);

        // Set two different tag metadata entries.
        txn.set_tag_metadata("work".into(), r#"{"color":"#ff0000"}"#.into())
            .await?;
        txn.set_tag_metadata("home".into(), r#"{"color":"#00ff00"}"#.into())
            .await?;
        txn.commit().await?;
    }
    {
        // Read back — verify isolation between tags.
        let mut txn = storage.txn().await?;
        assert_eq!(
            txn.get_tag_metadata("work".into()).await?,
            Some(r#"{"color":"#ff0000"}"#.into())
        );
        assert_eq!(
            txn.get_tag_metadata("home".into()).await?,
            Some(r#"{"color":"#00ff00"}"#.into())
        );
        assert_eq!(txn.get_tag_metadata("nonexistent".into()).await?, None);
    }
    Ok(())
}

pub(super) async fn tag_metadata_update(mut storage: impl Storage) -> Result<()> {
    {
        let mut txn = storage.txn().await?;
        txn.set_tag_metadata("work".into(), r#"{"color":"#ff0000"}"#.into())
            .await?;
        txn.commit().await?;
    }
    {
        let mut txn = storage.txn().await?;
        txn.set_tag_metadata("work".into(), r#"{"color":"#00ff00"}"#.into())
            .await?;
        txn.commit().await?;
    }
    {
        let mut txn = storage.txn().await?;
        assert_eq!(
            txn.get_tag_metadata("work".into()).await?,
            Some(r#"{"color":"#00ff00"}"#.into())
        );
    }
    Ok(())
}

pub(super) async fn get_all_tags(mut storage: impl Storage) -> Result<()> {
    // Empty storage returns empty vec.
    {
        let mut txn = storage.txn().await?;
        let tags = txn.get_all_tags().await?;
        assert!(tags.is_empty(), "no tags before any tasks exist");
        txn.commit().await?;
    }
    // Two tasks with overlapping tags — deduplication check.
    {
        let mut txn = storage.txn().await?;
        txn.set_task(
            Uuid::new_v4(),
            taskmap_with(vec![
                ("tag_work".to_string(), String::new()),
                ("tag_urgent".to_string(), String::new()),
            ]),
        )
        .await?;
        txn.set_task(
            Uuid::new_v4(),
            taskmap_with(vec![
                ("tag_work".to_string(), String::new()),
                ("tag_home".to_string(), String::new()),
            ]),
        )
        .await?;
        txn.commit().await?;
    }
    {
        let mut txn = storage.txn().await?;
        let tags = txn.get_all_tags().await?;
        // "work" appears in both tasks but should only appear once.
        assert_eq!(tags, vec!["home", "urgent", "work"], "sorted, deduplicated");
    }
    Ok(())
}

pub(super) async fn task_operations(mut storage: impl Storage) -> Result<()> {
    let uuid1 = Uuid::new_v4();
    let uuid2 = Uuid::new_v4();
    let uuid3 = Uuid::new_v4();
    let now = Utc::now();

    // Create some tasks and operations.
    {
        let mut txn = storage.txn().await?;

        txn.create_task(uuid1).await?;
        txn.create_task(uuid2).await?;
        txn.create_task(uuid3).await?;

        txn.add_operation(Operation::UndoPoint).await?;
        txn.add_operation(Operation::Create { uuid: uuid1 }).await?;
        txn.add_operation(Operation::Create { uuid: uuid1 }).await?;
        txn.add_operation(Operation::UndoPoint).await?;
        txn.add_operation(Operation::Delete {
            uuid: uuid2,
            old_task: TaskMap::new(),
        })
        .await?;
        txn.add_operation(Operation::Update {
            uuid: uuid3,
            property: "p".into(),
            old_value: None,
            value: Some("P".into()),
            timestamp: now,
        })
        .await?;
        txn.add_operation(Operation::Delete {
            uuid: uuid3,
            old_task: TaskMap::new(),
        })
        .await?;

        txn.commit().await?;
    }

    // remove the last operation to verify it doesn't appear
    {
        let mut txn = storage.txn().await?;
        txn.remove_operation(Operation::Delete {
            uuid: uuid3,
            old_task: TaskMap::new(),
        })
        .await?;
        txn.commit().await?;
    }

    // read them back
    {
        let mut txn = storage.txn().await?;
        let ops = txn.get_task_operations(uuid1).await?;
        assert_eq!(
            ops,
            vec![
                Operation::Create { uuid: uuid1 },
                Operation::Create { uuid: uuid1 },
            ]
        );
        let ops = txn.get_task_operations(uuid2).await?;
        assert_eq!(
            ops,
            vec![Operation::Delete {
                uuid: uuid2,
                old_task: TaskMap::new()
            }]
        );
        let ops = txn.get_task_operations(uuid3).await?;
        assert_eq!(
            ops,
            vec![Operation::Update {
                uuid: uuid3,
                property: "p".into(),
                old_value: None,
                value: Some("P".into()),
                timestamp: now,
            }]
        );
    }

    Ok(())
}
