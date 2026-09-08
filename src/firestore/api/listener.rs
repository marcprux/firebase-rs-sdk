//! Snapshot listeners (`onSnapshot`) backed by the Firestore `Listen` streaming RPC.
//!
//! A listener opens a gRPC stream, registers one target, and folds the server's watch changes into
//! snapshots with the same pieces the JS SDK uses: [`WatchChangeAggregator`] turns the raw changes
//! into `RemoteEvent`s, and the local view turns those into `QuerySnapshot`s with document changes.
//!
//! Only what the server sends is reflected: there is no local mutation queue, so
//! `has_pending_writes` is always false and a local write shows up when the backend echoes it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::firestore::api::query::{
    compute_doc_changes, OrderBy, Query, QueryDefinition, QueryDocumentChange, QuerySnapshot, QuerySnapshotMetadata,
};
use crate::firestore::api::snapshot::{DocumentSnapshot, SnapshotMetadata};
use crate::firestore::error::{FirestoreError, FirestoreResult};
use crate::firestore::model::{DocumentKey, Timestamp};
use crate::firestore::query_evaluator::compare_snapshots;
use crate::firestore::remote::listen::{
    decode_listen_response, is_retryable_status, map_status, ListenCancellation, ListenSession, ListenTarget,
    ListenTransport, LISTEN_TARGET_ID,
};
use crate::firestore::remote::remote_event::RemoteEvent;
use crate::firestore::remote::watch_change::{TargetChangeState, WatchChange, WatchDocument};
use crate::firestore::remote::watch_change_aggregator::WatchChangeAggregator;
use crate::platform::runtime;

/// Backoff bounds for reconnecting a dropped stream, mirroring the JS SDK's stream backoff.
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
const BACKOFF_FACTOR: u32 = 2;

/// Handle returned by the `on_snapshot` helpers.
///
/// The listener runs until this handle is dropped or [`remove`](Self::remove) is called, so keep it
/// alive for as long as the callbacks should fire.
#[must_use = "the listener is detached as soon as this registration is dropped"]
pub struct ListenerRegistration {
    cancellation: ListenCancellation,
}

impl ListenerRegistration {
    pub(crate) fn new(cancellation: ListenCancellation) -> Self {
        Self { cancellation }
    }

    /// Detaches the listener; no further callbacks are delivered.
    pub fn remove(self) {
        // `Drop` does the work.
    }
}

impl Drop for ListenerRegistration {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl std::fmt::Debug for ListenerRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListenerRegistration").finish()
    }
}

/// Callback types kept short for readability.
type QueryCallback = Arc<dyn Fn(FirestoreResult<QuerySnapshot>) + Send + Sync + 'static>;
type DocumentCallback = Arc<dyn Fn(FirestoreResult<DocumentSnapshot>) + Send + Sync + 'static>;

/// Starts a query listener and returns its registration.
pub(crate) fn listen_to_query(
    transport: Arc<ListenTransport>,
    query: Query,
    callback: QueryCallback,
) -> ListenerRegistration {
    let cancellation = ListenCancellation::default();
    let task_cancellation = cancellation.clone();
    runtime::spawn_detached(async move {
        run_listener(
            transport,
            ListenTarget::Query(query.definition()),
            Box::new(QueryViewSink::new(query, callback)),
            task_cancellation,
        )
        .await;
    });
    ListenerRegistration::new(cancellation)
}

/// Starts a single-document listener and returns its registration.
pub(crate) fn listen_to_document(
    transport: Arc<ListenTransport>,
    key: DocumentKey,
    callback: DocumentCallback,
) -> ListenerRegistration {
    let cancellation = ListenCancellation::default();
    let task_cancellation = cancellation.clone();
    runtime::spawn_detached(async move {
        run_listener(
            transport,
            ListenTarget::Document(key.clone()),
            Box::new(DocumentViewSink::new(key, callback)),
            task_cancellation,
        )
        .await;
    });
    ListenerRegistration::new(cancellation)
}

/// What a listener does with the events it receives; one implementation per snapshot flavour.
trait ViewSink: Send {
    /// Folds one remote event into the view and delivers a snapshot when something changed.
    fn apply(&mut self, event: RemoteEvent);
    /// Reports a terminal error to the user.
    fn fail(&self, error: FirestoreError);
    /// Drops everything the view knows, used when the stream restarts without a resume token.
    fn reset(&mut self);
}

async fn run_listener(
    transport: Arc<ListenTransport>,
    target: ListenTarget,
    mut sink: Box<dyn ViewSink>,
    cancellation: ListenCancellation,
) {
    let mut resume_token: Option<Vec<u8>> = None;
    let mut backoff = INITIAL_BACKOFF;

    loop {
        if cancellation.is_cancelled() {
            return;
        }

        let session = match transport.open(&target, resume_token.clone()).await {
            Ok(session) => session,
            Err(error) => {
                sink.fail(error);
                return;
            }
        };

        match pump_stream(&transport, session, &mut sink, &cancellation, &mut resume_token).await {
            StreamOutcome::Cancelled | StreamOutcome::Fatal => return,
            StreamOutcome::Retry => {}
        }

        // The stream dropped for a reason worth retrying; resume from the last token when we have
        // one, otherwise start the view over because the backend replays from scratch.
        if resume_token.is_none() {
            sink.reset();
        }
        runtime::sleep(backoff).await;
        backoff = (backoff * BACKOFF_FACTOR).min(MAX_BACKOFF);
    }
}

enum StreamOutcome {
    Cancelled,
    Fatal,
    Retry,
}

async fn pump_stream(
    transport: &ListenTransport,
    mut session: ListenSession,
    sink: &mut Box<dyn ViewSink>,
    cancellation: &ListenCancellation,
    resume_token: &mut Option<Vec<u8>>,
) -> StreamOutcome {
    let mut aggregator: WatchChangeAggregator = WatchChangeAggregator::new(Arc::new(()));
    // Set when the server sends an existence filter, so a reset in the next snapshot can be told
    // apart from a server-initiated RESET (which is followed by the documents on the same stream).
    let mut saw_existence_filter = false;

    loop {
        let message = tokio::select! {
            _ = cancellation.cancelled() => return StreamOutcome::Cancelled,
            message = session.next() => message,
        };

        let response = match message {
            Ok(Some(response)) => response,
            // A clean end of stream is normal for long-lived listens; reconnect.
            Ok(None) => return StreamOutcome::Retry,
            Err(status) => {
                if is_retryable_status(&status) {
                    return StreamOutcome::Retry;
                }
                sink.fail(map_status(&status));
                return StreamOutcome::Fatal;
            }
        };

        let change = match decode_listen_response(transport.serializer(), &response) {
            Ok(Some(change)) => change,
            Ok(None) => continue,
            Err(error) => {
                sink.fail(error);
                return StreamOutcome::Fatal;
            }
        };

        // The server reports a rejected target (rules, missing index, ...) as a REMOVE carrying the
        // cause; that is permanent, so hand it to the caller instead of reconnecting forever.
        if let WatchChange::TargetChange(target_change) = &change {
            if let Some(cause) = target_change.cause.clone() {
                sink.fail(cause);
                return StreamOutcome::Fatal;
            }
            if let Some(token) = target_change.resume_token.clone() {
                *resume_token = Some(token);
            }
        }

        if matches!(change, WatchChange::ExistenceFilter(_)) {
            saw_existence_filter = true;
        }

        let snapshot_boundary = snapshot_read_time(&change);

        if let Err(error) = aggregator.handle_watch_change(change) {
            sink.fail(error);
            return StreamOutcome::Fatal;
        }

        if let Some(read_time) = snapshot_boundary {
            aggregator.set_snapshot_version(Some(read_time));
            let event = aggregator.drain();
            // An existence filter that did not match means the local view drifted; the backend does
            // not resend the target's documents for it, so the stream has to be reopened without a
            // resume token. A server-initiated RESET needs no such thing: the documents follow on
            // the same stream.
            let filter_mismatch = saw_existence_filter && event.target_resets.contains(&LISTEN_TARGET_ID);
            saw_existence_filter = false;
            if !event.is_empty() {
                sink.apply(event);
            }
            if filter_mismatch {
                *resume_token = None;
                return StreamOutcome::Retry;
            }
        }
    }
}

/// A global `NO_CHANGE` (no target ids) carrying a read time means "everything up to here has been
/// delivered", which is where the JS SDK raises a snapshot.
fn snapshot_read_time(change: &WatchChange) -> Option<Timestamp> {
    match change {
        WatchChange::TargetChange(target_change)
            if target_change.state == TargetChangeState::NoChange && target_change.target_ids.is_empty() =>
        {
            target_change.read_time
        }
        _ => None,
    }
}

/// Document data as the view keeps it, before it is wrapped in a snapshot with fresh metadata.
#[derive(Clone)]
struct ViewDocument {
    document: WatchDocument,
}

impl ViewDocument {
    fn snapshot(&self, from_cache: bool) -> DocumentSnapshot {
        DocumentSnapshot::new(
            self.document.key.clone(),
            Some(self.document.fields.clone()),
            SnapshotMetadata::new(from_cache, false),
        )
        .with_times(self.document.create_time, self.document.update_time)
    }
}

/// Turns remote events into `QuerySnapshot`s.
struct QueryViewSink {
    query: Query,
    definition: QueryDefinition,
    callback: QueryCallback,
    documents: BTreeMap<DocumentKey, ViewDocument>,
    current: bool,
    raised_first_snapshot: bool,
    previous: Option<Vec<DocumentSnapshot>>,
}

impl QueryViewSink {
    fn new(query: Query, callback: QueryCallback) -> Self {
        let definition = query.definition();
        Self {
            query,
            definition,
            callback,
            documents: BTreeMap::new(),
            current: false,
            raised_first_snapshot: false,
            previous: None,
        }
    }

    fn sorted_documents(&self, from_cache: bool) -> Vec<DocumentSnapshot> {
        let mut documents: Vec<DocumentSnapshot> = self
            .documents
            .values()
            .map(|document| document.snapshot(from_cache))
            .collect();
        let order_by: &[OrderBy] = self.definition.result_order_by();
        documents.sort_by(|left, right| compare_snapshots(left, right, order_by));
        documents
    }
}

impl ViewSink for QueryViewSink {
    fn apply(&mut self, event: RemoteEvent) {
        let was_current = self.current;
        // Remember each document's version so the diff below can tell "changed" from "moved".
        let previous_versions: BTreeMap<DocumentKey, Option<Timestamp>> = self
            .documents
            .iter()
            .map(|(key, document)| (key.clone(), document.document.update_time))
            .collect();

        if event.target_resets.contains(&LISTEN_TARGET_ID) {
            self.documents.clear();
            self.current = false;
        }

        for (key, document) in &event.document_updates {
            match document {
                Some(document) => {
                    self.documents.insert(
                        key.clone(),
                        ViewDocument {
                            document: document.clone(),
                        },
                    );
                }
                None => {
                    self.documents.remove(key);
                }
            }
        }

        let mut resume_token = None;
        if let Some(change) = event.target_changes.get(&LISTEN_TARGET_ID) {
            // Documents that fell out of the query window are removed even though they still exist.
            for key in &change.removed_documents {
                self.documents.remove(key);
            }
            if change.current {
                self.current = true;
            }
            resume_token = change.resume_token.clone();
        }

        let sync_state_changed = was_current != self.current;
        let from_cache = !self.current;
        let documents = self.sorted_documents(from_cache);
        let changes = if self.raised_first_snapshot {
            // Report what actually changed. Diffing positions instead (as the one-shot path must)
            // would also flag every document that merely shifted because a neighbour disappeared,
            // which the JS SDK does not do.
            document_changes(&previous_versions, self.previous.as_deref(), &documents)
        } else {
            compute_doc_changes(None, &documents)
        };

        // The first snapshot is raised as soon as the target is in sync, even when it is empty;
        // afterwards only real changes are reported.
        let should_raise = if !self.raised_first_snapshot {
            self.current
        } else {
            !changes.is_empty() || sync_state_changed
        };

        if !should_raise {
            return;
        }

        self.raised_first_snapshot = true;
        self.previous = Some(documents.clone());

        let metadata =
            QuerySnapshotMetadata::new(from_cache, false, sync_state_changed, resume_token, event.snapshot_version);
        let snapshot = QuerySnapshot::new(self.query.clone(), documents, metadata, changes);
        (self.callback)(Ok(snapshot));
    }

    fn fail(&self, error: FirestoreError) {
        (self.callback)(Err(error));
    }

    fn reset(&mut self) {
        self.documents.clear();
        self.current = false;
        self.previous = None;
        self.raised_first_snapshot = false;
    }
}

/// Builds the document changes for a snapshot from what actually changed, using the previous and
/// current orderings only to fill in `old_index` / `new_index`.
fn document_changes(
    previous_versions: &BTreeMap<DocumentKey, Option<Timestamp>>,
    previous: Option<&[DocumentSnapshot]>,
    current: &[DocumentSnapshot],
) -> Vec<QueryDocumentChange> {
    let previous = previous.unwrap_or(&[]);
    let old_index_of: BTreeMap<&DocumentKey, usize> = previous
        .iter()
        .enumerate()
        .map(|(index, doc)| (doc.document_key(), index))
        .collect();

    let mut changes = Vec::new();
    for (new_index, doc) in current.iter().enumerate() {
        let key = doc.document_key();
        match previous_versions.get(key) {
            None => changes.push(QueryDocumentChange::added(doc.clone(), new_index)),
            Some(previous_version) => {
                if *previous_version != doc.update_time() {
                    let old_index = old_index_of.get(key).copied().unwrap_or(new_index);
                    changes.push(QueryDocumentChange::modified(doc.clone(), old_index, new_index));
                }
            }
        }
    }

    let still_present: std::collections::BTreeSet<&DocumentKey> =
        current.iter().map(|doc| doc.document_key()).collect();
    for (index, doc) in previous.iter().enumerate() {
        if !still_present.contains(doc.document_key()) {
            changes.push(QueryDocumentChange::removed(doc.clone(), index));
        }
    }

    changes
}

/// Turns remote events into `DocumentSnapshot`s for a single document.
struct DocumentViewSink {
    key: DocumentKey,
    callback: DocumentCallback,
    document: Option<ViewDocument>,
    current: bool,
    raised_first_snapshot: bool,
}

impl DocumentViewSink {
    fn new(key: DocumentKey, callback: DocumentCallback) -> Self {
        Self {
            key,
            callback,
            document: None,
            current: false,
            raised_first_snapshot: false,
        }
    }
}

impl ViewSink for DocumentViewSink {
    fn apply(&mut self, event: RemoteEvent) {
        let was_current = self.current;
        let previous_exists = self.document.is_some();
        let previous_update_time = self.document.as_ref().and_then(|doc| doc.document.update_time);

        if event.target_resets.contains(&LISTEN_TARGET_ID) {
            self.document = None;
            self.current = false;
        }

        if let Some(update) = event.document_updates.get(&self.key) {
            self.document = update.clone().map(|document| ViewDocument { document });
        }

        if let Some(change) = event.target_changes.get(&LISTEN_TARGET_ID) {
            if change.removed_documents.contains(&self.key) {
                self.document = None;
            }
            if change.current {
                self.current = true;
            }
        }

        let sync_state_changed = was_current != self.current;
        let changed = previous_exists != self.document.is_some()
            || previous_update_time != self.document.as_ref().and_then(|doc| doc.document.update_time);

        let should_raise = if !self.raised_first_snapshot {
            self.current
        } else {
            changed || sync_state_changed
        };

        if !should_raise {
            return;
        }

        self.raised_first_snapshot = true;
        let from_cache = !self.current;
        let snapshot = match &self.document {
            Some(document) => document.snapshot(from_cache),
            None => DocumentSnapshot::new(self.key.clone(), None, SnapshotMetadata::new(from_cache, false)),
        };
        (self.callback)(Ok(snapshot));
    }

    fn fail(&self, error: FirestoreError) {
        (self.callback)(Err(error));
    }

    fn reset(&mut self) {
        self.document = None;
        self.current = false;
        self.raised_first_snapshot = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firestore::model::ResourcePath;
    use crate::firestore::value::MapValue;

    fn snapshot(id: &str, update_time: i64) -> DocumentSnapshot {
        let key = DocumentKey::from_string(&format!("rooms/{id}")).expect("key");
        DocumentSnapshot::new(key, Some(MapValue::new(BTreeMap::new())), SnapshotMetadata::new(false, false))
            .with_times(None, Some(Timestamp::new(update_time, 0)))
    }

    fn versions(entries: &[(&str, i64)]) -> BTreeMap<DocumentKey, Option<Timestamp>> {
        entries
            .iter()
            .map(|(id, time)| {
                (
                    DocumentKey::from_string(&format!("rooms/{id}")).expect("key"),
                    Some(Timestamp::new(*time, 0)),
                )
            })
            .collect()
    }

    #[test]
    fn new_documents_are_reported_as_added() {
        let current = vec![snapshot("a", 1), snapshot("b", 1)];
        let changes = document_changes(&versions(&[("a", 1)]), Some(&[snapshot("a", 1)]), &current);

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type(), crate::firestore::DocumentChangeType::Added);
        assert_eq!(changes[0].doc().id(), "b");
        assert_eq!(changes[0].new_index(), 1);
    }

    #[test]
    fn rewritten_documents_are_reported_as_modified_with_both_indexes() {
        let previous = vec![snapshot("a", 1), snapshot("b", 1)];
        // `a` was rewritten and now sorts after `b`.
        let current = vec![snapshot("b", 1), snapshot("a", 2)];
        let changes = document_changes(&versions(&[("a", 1), ("b", 1)]), Some(&previous), &current);

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type(), crate::firestore::DocumentChangeType::Modified);
        assert_eq!(changes[0].doc().id(), "a");
        assert_eq!(changes[0].old_index(), 0);
        assert_eq!(changes[0].new_index(), 1);
    }

    #[test]
    fn documents_that_only_shift_position_are_not_reported() {
        let previous = vec![snapshot("a", 1), snapshot("b", 1), snapshot("c", 1)];
        // `b` disappeared; `c` moved up but did not change.
        let current = vec![snapshot("a", 1), snapshot("c", 1)];
        let changes = document_changes(&versions(&[("a", 1), ("b", 1), ("c", 1)]), Some(&previous), &current);

        assert_eq!(
            changes.len(),
            1,
            "only the removal is a change, got {:?}",
            changes
                .iter()
                .map(|c| (c.change_type(), c.doc().id().to_string()))
                .collect::<Vec<_>>()
        );
        assert_eq!(changes[0].change_type(), crate::firestore::DocumentChangeType::Removed);
        assert_eq!(changes[0].doc().id(), "b");
        assert_eq!(changes[0].old_index(), 1);
    }

    #[test]
    fn document_keys_survive_the_round_trip_through_resource_paths() {
        let key = DocumentKey::from_path(ResourcePath::from_string("rooms/lobby").expect("path")).expect("key");
        assert_eq!(key.path().canonical_string(), "rooms/lobby");
    }
}
