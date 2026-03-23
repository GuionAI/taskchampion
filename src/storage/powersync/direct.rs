//! Direct (non-Wrapper) PowerSync storage for FFI use.
//!
//! Bypasses the actor-thread pattern used by [`super::PowerSyncStorage`]. Sound only
//! when the caller guarantees single-threaded access (e.g., a `current_thread` tokio
//! runtime in an FFI bridge).

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use async_trait::async_trait;
use uuid::Uuid;

use super::inner::{PowerSyncStorageInner, SendPtr};
use crate::errors::Result;
use crate::operation::Operation;
use crate::storage::send_wrapper::{WrappedStorage, WrappedStorageTxn};
use crate::storage::{Storage, StorageTxn, TaskMap};

// ---------------------------------------------------------------------------
// SendFut — marks a !Send future as Send for single-threaded runtimes
// ---------------------------------------------------------------------------

/// Wraps a `!Send` future and declares it `Send`.
///
/// # Safety
///
/// Only sound when polled exclusively on a single thread (e.g., a
/// `current_thread` Tokio runtime). The future is **never** moved across
/// threads at runtime; we only need to satisfy the compile-time bound.
struct SendFut<F>(F);

// SAFETY: We guarantee single-threaded access via the `current_thread` runtime
// in the FFI layer. The future is never actually sent to another thread.
unsafe impl<F: Future> Send for SendFut<F> {}

impl<F: Future> Future for SendFut<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: We never move the inner future; we only re-pin it.
        unsafe { self.map_unchecked_mut(|s| &mut s.0) }.poll(cx)
    }
}

// ---------------------------------------------------------------------------
// DirectPowerSyncStorage
// ---------------------------------------------------------------------------

/// FFI-optimised PowerSync storage that bypasses the Wrapper actor thread.
///
/// # Safety Contract
///
/// The caller **must** guarantee single-threaded access. This is typically
/// achieved by driving all FFI calls through a `current_thread` Tokio runtime.
/// Using this type with a multi-thread runtime is **unsound**.
pub struct DirectPowerSyncStorage(PowerSyncStorageInner);

// SAFETY: The FFI layer guarantees single-threaded access via a
// `current_thread` runtime, so the `!Send` rusqlite Connection inside
// `PowerSyncStorageInner` is never accessed from multiple threads.
unsafe impl Send for DirectPowerSyncStorage {}

impl DirectPowerSyncStorage {
    /// Create from an existing SQLite handle (non-owning).
    ///
    /// # Safety
    ///
    /// - `handle` must be a valid, open `sqlite3*` pointer.
    /// - The caller retains ownership; this storage will **not** close the
    ///   handle on drop.
    /// - The handle must remain valid for the entire lifetime of this storage.
    /// - Must only be used with a single-threaded async runtime.
    pub unsafe fn from_handle(handle: *mut rusqlite::ffi::sqlite3, user_id: Uuid) -> Result<Self> {
        let inner = PowerSyncStorageInner::from_handle(SendPtr(handle), user_id)?;
        Ok(Self(inner))
    }
}

// ---------------------------------------------------------------------------
// DirectTxn — thin !Send→Send bridge for the transaction
// ---------------------------------------------------------------------------

/// Thin wrapper that makes a `Box<dyn WrappedStorageTxn>` satisfy `Send`.
///
/// # Safety Contract
///
/// Same single-threaded guarantee as [`DirectPowerSyncStorage`].
struct DirectTxn<'a>(Box<dyn WrappedStorageTxn + 'a>);

// SAFETY: Same single-threaded guarantee as DirectPowerSyncStorage.
unsafe impl Send for DirectTxn<'_> {}

// ---------------------------------------------------------------------------
// Storage impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Storage for DirectPowerSyncStorage {
    async fn txn<'a>(&'a mut self) -> Result<Box<dyn StorageTxn + Send + 'a>> {
        let inner_txn = SendFut(WrappedStorage::txn(&mut self.0)).await?;
        Ok(Box::new(DirectTxn(inner_txn)))
    }
}

// ---------------------------------------------------------------------------
// StorageTxn impl — delegates every method via SendFut
// ---------------------------------------------------------------------------

#[async_trait]
impl StorageTxn for DirectTxn<'_> {
    async fn get_task(&mut self, uuid: Uuid) -> Result<Option<TaskMap>> {
        SendFut(self.0.get_task(uuid)).await
    }

    async fn get_pending_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        SendFut(self.0.get_pending_tasks()).await
    }

    async fn create_task(&mut self, uuid: Uuid) -> Result<bool> {
        SendFut(self.0.create_task(uuid)).await
    }

    async fn set_task(&mut self, uuid: Uuid, task: TaskMap) -> Result<()> {
        SendFut(self.0.set_task(uuid, task)).await
    }

    async fn delete_task(&mut self, uuid: Uuid) -> Result<bool> {
        SendFut(self.0.delete_task(uuid)).await
    }

    async fn all_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        SendFut(self.0.all_tasks()).await
    }

    async fn all_task_uuids(&mut self) -> Result<Vec<Uuid>> {
        SendFut(self.0.all_task_uuids()).await
    }

    async fn get_task_operations(&mut self, uuid: Uuid) -> Result<Vec<Operation>> {
        SendFut(self.0.get_task_operations(uuid)).await
    }

    async fn all_operations(&mut self) -> Result<Vec<Operation>> {
        SendFut(self.0.all_operations()).await
    }

    async fn add_operation(&mut self, op: Operation) -> Result<()> {
        SendFut(self.0.add_operation(op)).await
    }

    async fn remove_operation(&mut self, op: Operation) -> Result<()> {
        SendFut(self.0.remove_operation(op)).await
    }

    async fn commit(&mut self) -> Result<()> {
        SendFut(self.0.commit()).await
    }
}
