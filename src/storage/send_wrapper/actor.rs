use super::{WrappedStorage, WrappedStorageTxn};
use crate::errors::Result;
use crate::operation::Operation;
use crate::storage::TaskMap;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// An enum for messages sent to the sync thread actor.
pub(super) enum ActorMessage {
    // Transaction control
    BeginTxn(oneshot::Sender<Result<mpsc::UnboundedSender<TxnMessage>>>),
}

pub(super) enum TxnMessage {
    Commit(oneshot::Sender<Result<()>>),
    Rollback,

    // Transactional operations
    GetTask(Uuid, oneshot::Sender<Result<Option<TaskMap>>>),
    GetPendingTasks(oneshot::Sender<Result<Vec<(Uuid, TaskMap)>>>),
    CreateTask(Uuid, oneshot::Sender<Result<bool>>),
    SetTask(Uuid, TaskMap, oneshot::Sender<Result<()>>),
    DeleteTask(Uuid, oneshot::Sender<Result<bool>>),
    AllTasks(oneshot::Sender<Result<Vec<(Uuid, TaskMap)>>>),
    AllTaskUuids(oneshot::Sender<Result<Vec<Uuid>>>),
    GetTaskOperations(Uuid, oneshot::Sender<Result<Vec<Operation>>>),
    AllOperations(oneshot::Sender<Result<Vec<Operation>>>),
    AddOperation(Operation, oneshot::Sender<Result<()>>),
    RemoveOperation(Operation, oneshot::Sender<Result<()>>),
    IsEmpty(oneshot::Sender<Result<bool>>),
    GetTagMetadata(String, oneshot::Sender<Result<Option<String>>>),
    SetTagMetadata(String, String, oneshot::Sender<Result<()>>),
    GetAllTags(oneshot::Sender<Result<Vec<String>>>),
}

/// State owned by the dedicated thread. It handles the various channels and
/// delegates to the inner storage.
pub(super) struct ActorImpl<S: WrappedStorage> {
    storage: S,
    receiver: mpsc::UnboundedReceiver<ActorMessage>,
}

impl<S: WrappedStorage> ActorImpl<S> {
    pub(super) fn new(storage: S, receiver: mpsc::UnboundedReceiver<ActorMessage>) -> Self {
        Self { storage, receiver }
    }

    pub(super) async fn run(&mut self) {
        // The outer loop waits for a BeginTxn message. If the channel is disconnected, the thread
        // will exit gracefully. Note that this loop blocks until the transaction is complete,
        // effectively ensuring serialized access (and simplifying management of ownership).
        while let Some(ActorMessage::BeginTxn(reply_sender)) = self.receiver.recv().await {
            let (txn_sender, mut txn_receiver) = mpsc::unbounded_channel::<TxnMessage>();
            match self.storage.txn().await {
                Ok(mut txn) => {
                    // Send the new transaction channel sender back
                    if reply_sender.send(Ok(txn_sender)).is_err() {
                        continue; // Don't handle the txn if the client is gone.
                    }
                    Self::handle_transaction(&mut txn_receiver, &mut txn).await;
                }
                Err(e) => {
                    // Send the database error back to the caller
                    let _ = reply_sender.send(Err(e));
                }
            }
        }
    }

    /// The inner loop for handling messages within an active transaction.
    async fn handle_transaction(
        receiver: &mut mpsc::UnboundedReceiver<TxnMessage>,
        txn: &mut Box<dyn WrappedStorageTxn + '_>,
    ) {
        while let Some(msg) = receiver.recv().await {
            match msg {
                TxnMessage::Commit(resp) => {
                    let _ = resp.send(txn.commit().await);
                    return; // Transaction over, return to the outer loop.
                }
                TxnMessage::Rollback => {
                    return; // Transaction over, return to the outer loop.
                }
                TxnMessage::GetTask(uuid, resp) => {
                    let _ = resp.send(txn.get_task(uuid).await);
                }
                TxnMessage::GetPendingTasks(resp) => {
                    let _ = resp.send(txn.get_pending_tasks().await);
                }
                TxnMessage::CreateTask(uuid, resp) => {
                    let _ = resp.send(txn.create_task(uuid).await);
                }
                TxnMessage::SetTask(uuid, t, resp) => {
                    let _ = resp.send(txn.set_task(uuid, t).await);
                }
                TxnMessage::DeleteTask(uuid, resp) => {
                    let _ = resp.send(txn.delete_task(uuid).await);
                }
                TxnMessage::AllTasks(resp) => {
                    let _ = resp.send(txn.all_tasks().await);
                }
                TxnMessage::AllTaskUuids(resp) => {
                    let _ = resp.send(txn.all_task_uuids().await);
                }
                TxnMessage::GetTaskOperations(u, resp) => {
                    let _ = resp.send(txn.get_task_operations(u).await);
                }
                TxnMessage::AllOperations(resp) => {
                    let _ = resp.send(txn.all_operations().await);
                }
                TxnMessage::AddOperation(o, resp) => {
                    let _ = resp.send(txn.add_operation(o).await);
                }
                TxnMessage::RemoveOperation(o, resp) => {
                    let _ = resp.send(txn.remove_operation(o).await);
                }
                TxnMessage::IsEmpty(resp) => {
                    let _ = resp.send(txn.is_empty().await);
                }
                TxnMessage::GetTagMetadata(name, resp) => {
                    let _ = resp.send(txn.get_tag_metadata(name).await);
                }
                TxnMessage::SetTagMetadata(name, data, resp) => {
                    let _ = resp.send(txn.set_tag_metadata(name, data).await);
                }
                TxnMessage::GetAllTags(resp) => {
                    let _ = resp.send(txn.get_all_tags().await);
                }
            };
        }
    }
}
