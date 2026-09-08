use std::cmp;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::error::{canceled, internal_error, StorageError, StorageResult};
use crate::metadata::serde::ObjectMetadata;
use crate::reference::StorageReference;
use crate::request::{
    cancel_resumable_upload_request, continue_resumable_upload_request, create_resumable_upload_request,
    get_resumable_upload_status_request, multipart_upload_request, RESUMABLE_UPLOAD_CHUNK_SIZE,
};
use crate::UploadMetadata;
use firebase_core::platform::runtime;

const MAX_RESUMABLE_CHUNK_SIZE: usize = 32 * 1024 * 1024;

/// How long [`UploadTask::run_to_completion`] waits between checks while the task is paused.
const DEFAULT_PAUSE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Represents the execution state of an [`UploadTask`].
///
/// The variants line up with the Web SDK's `TaskState` (`running`, `paused`, `success`, `canceled`,
/// `error`); [`UploadTaskState::Pending`] additionally covers the window between creating a task
/// and uploading its first chunk, which the Web SDK does not expose because it starts uploading
/// immediately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadTaskState {
    Pending,
    Running,
    Paused,
    Completed,
    Error,
    Canceled,
}

impl UploadTaskState {
    /// The Web SDK `TaskState` string for this state.
    pub fn as_str(self) -> &'static str {
        match self {
            UploadTaskState::Pending => "pending",
            UploadTaskState::Running => "running",
            UploadTaskState::Paused => "paused",
            UploadTaskState::Completed => "success",
            UploadTaskState::Error => "error",
            UploadTaskState::Canceled => "canceled",
        }
    }

    /// True once the task can no longer make progress.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            UploadTaskState::Completed | UploadTaskState::Error | UploadTaskState::Canceled
        )
    }
}

impl fmt::Display for UploadTaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Progress information emitted while uploading large blobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadProgress {
    pub bytes_transferred: u64,
    pub total_bytes: u64,
}

impl UploadProgress {
    pub fn new(bytes_transferred: u64, total_bytes: u64) -> Self {
        Self {
            bytes_transferred,
            total_bytes,
        }
    }
}

/// Immutable view of an upload's progress, mirroring the Web SDK's `UploadTaskSnapshot`.
#[derive(Clone)]
pub struct UploadTaskSnapshot {
    pub bytes_transferred: u64,
    pub total_bytes: u64,
    pub state: UploadTaskState,
    /// Object metadata, available once the upload finished.
    pub metadata: Option<ObjectMetadata>,
    /// The reference the data is being uploaded to.
    pub reference: StorageReference,
}

impl UploadTaskSnapshot {
    /// The snapshot rendered as a plain [`UploadProgress`] pair.
    pub fn progress(&self) -> UploadProgress {
        UploadProgress::new(self.bytes_transferred, self.total_bytes)
    }
}

impl fmt::Debug for UploadTaskSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadTaskSnapshot")
            .field("path", &self.reference.full_path())
            .field("bytes_transferred", &self.bytes_transferred)
            .field("total_bytes", &self.total_bytes)
            .field("state", &self.state)
            .finish()
    }
}

type StateObserver = Box<dyn FnMut(UploadTaskSnapshot) + Send + 'static>;

#[derive(Default)]
struct ObserverRegistry {
    next_id: u64,
    observers: Vec<(u64, StateObserver)>,
    /// Observers unsubscribed while a dispatch was in flight.
    removed: Vec<u64>,
    dispatching: bool,
    /// Set when a state change happens inside an observer callback.
    pending: bool,
}

struct TaskState {
    state: UploadTaskState,
    transferred: u64,
    total_bytes: u64,
    metadata: Option<ObjectMetadata>,
    error: Option<StorageError>,
    upload_url: Option<String>,
}

/// State shared between an [`UploadTask`] and every [`UploadTaskHandle`] created from it.
struct UploadShared {
    reference: StorageReference,
    state: Mutex<TaskState>,
    observers: Mutex<ObserverRegistry>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl UploadShared {
    fn new(reference: StorageReference, total_bytes: u64) -> Self {
        Self {
            reference,
            state: Mutex::new(TaskState {
                state: UploadTaskState::Pending,
                transferred: 0,
                total_bytes,
                metadata: None,
                error: None,
                upload_url: None,
            }),
            observers: Mutex::new(ObserverRegistry::default()),
        }
    }

    fn snapshot(&self) -> UploadTaskSnapshot {
        let state = lock(&self.state);
        UploadTaskSnapshot {
            bytes_transferred: state.transferred,
            total_bytes: state.total_bytes,
            state: state.state,
            metadata: state.metadata.clone(),
            reference: self.reference.clone(),
        }
    }

    fn state(&self) -> UploadTaskState {
        lock(&self.state).state
    }

    fn total_bytes(&self) -> u64 {
        lock(&self.state).total_bytes
    }

    fn bytes_transferred(&self) -> u64 {
        lock(&self.state).transferred
    }

    fn upload_url(&self) -> Option<String> {
        lock(&self.state).upload_url.clone()
    }

    fn set_upload_url(&self, url: String) {
        lock(&self.state).upload_url = Some(url);
    }

    fn error(&self) -> Option<StorageError> {
        lock(&self.state).error.clone()
    }

    fn metadata(&self) -> Option<ObjectMetadata> {
        lock(&self.state).metadata.clone()
    }

    fn set_transferred(&self, transferred: u64) {
        lock(&self.state).transferred = transferred;
    }

    /// Moves the task to `next` when the current state is in `from`, notifying observers.
    fn transition(&self, from: &[UploadTaskState], next: UploadTaskState) -> bool {
        {
            let mut state = lock(&self.state);
            if !from.contains(&state.state) || state.state == next {
                return false;
            }
            state.state = next;
        }
        self.emit();
        true
    }

    fn complete(&self, metadata: ObjectMetadata) {
        {
            let mut state = lock(&self.state);
            state.transferred = state.total_bytes;
            state.metadata = Some(metadata);
            state.state = UploadTaskState::Completed;
        }
        self.emit();
    }

    fn fail(&self, error: StorageError) {
        {
            let mut state = lock(&self.state);
            state.error = Some(error);
            state.state = UploadTaskState::Error;
        }
        self.emit();
    }

    /// Records the error that made a cancelled task stop, keeping the `Canceled` state.
    fn record_cancellation(&self, error: StorageError) {
        let changed = {
            let mut state = lock(&self.state);
            if state.error.is_some() {
                false
            } else {
                state.error = Some(error);
                true
            }
        };
        if changed {
            self.emit();
        }
    }

    fn add_observer(&self, observer: StateObserver) -> u64 {
        let mut registry = lock(&self.observers);
        registry.next_id += 1;
        let id = registry.next_id;
        registry.observers.push((id, observer));
        id
    }

    fn remove_observer(&self, id: u64) {
        let mut registry = lock(&self.observers);
        registry.observers.retain(|(existing, _)| *existing != id);
        if registry.dispatching {
            registry.removed.push(id);
        }
    }

    /// Delivers the current snapshot to every observer.
    ///
    /// Observers are invoked without holding any lock, so they may call back into
    /// [`UploadTaskHandle::pause`] and friends; state changes made from inside a callback are
    /// delivered in a follow-up round rather than recursively.
    fn emit(&self) {
        {
            let mut registry = lock(&self.observers);
            if registry.dispatching {
                registry.pending = true;
                return;
            }
            if registry.observers.is_empty() {
                return;
            }
            registry.dispatching = true;
        }

        loop {
            let snapshot = self.snapshot();
            let mut taken = {
                let mut registry = lock(&self.observers);
                registry.pending = false;
                std::mem::take(&mut registry.observers)
            };

            for (_, observer) in taken.iter_mut() {
                observer(snapshot.clone());
            }

            let mut registry = lock(&self.observers);
            let removed = std::mem::take(&mut registry.removed);
            let mut restored: Vec<(u64, StateObserver)> =
                taken.into_iter().filter(|(id, _)| !removed.contains(id)).collect();
            // Observers registered from inside a callback keep their registration order.
            restored.append(&mut registry.observers);
            registry.observers = restored;

            if !registry.pending || registry.observers.is_empty() {
                registry.pending = false;
                registry.dispatching = false;
                return;
            }
        }
    }
}

/// Cloneable control surface for a running [`UploadTask`].
///
/// A handle can be moved into another task (or an observer callback) to pause, resume or cancel an
/// upload that is being driven elsewhere, mirroring the Web SDK's `UploadTask` control methods.
#[derive(Clone)]
pub struct UploadTaskHandle {
    shared: Arc<UploadShared>,
}

impl UploadTaskHandle {
    /// Current task state.
    pub fn state(&self) -> UploadTaskState {
        self.shared.state()
    }

    /// Snapshot of the task's progress, matching the Web SDK's `UploadTask.snapshot`.
    pub fn snapshot(&self) -> UploadTaskSnapshot {
        self.shared.snapshot()
    }

    /// Bytes uploaded and acknowledged by the server so far.
    pub fn bytes_transferred(&self) -> u64 {
        self.shared.bytes_transferred()
    }

    /// Total number of bytes the task will upload.
    pub fn total_bytes(&self) -> u64 {
        self.shared.total_bytes()
    }

    /// Last error recorded by the task, if any.
    pub fn last_error(&self) -> Option<StorageError> {
        self.shared.error()
    }

    /// The resumable session URL, once the session has been opened.
    pub fn upload_session_url(&self) -> Option<String> {
        self.shared.upload_url()
    }

    /// Pauses the upload, returning `true` when the task actually changed state.
    ///
    /// The upload stops at the next chunk boundary; a chunk that is already in flight still
    /// completes.
    pub fn pause(&self) -> bool {
        self.shared
            .transition(&[UploadTaskState::Pending, UploadTaskState::Running], UploadTaskState::Paused)
    }

    /// Resumes a paused upload, returning `true` when the task actually changed state.
    pub fn resume(&self) -> bool {
        self.shared
            .transition(&[UploadTaskState::Paused], UploadTaskState::Running)
    }

    /// Cancels the upload, returning `true` when the task actually changed state.
    ///
    /// The driving call fails with `storage/canceled` at the next chunk boundary and the resumable
    /// session is discarded server-side, so no object is created.
    pub fn cancel(&self) -> bool {
        self.shared.transition(
            &[
                UploadTaskState::Pending,
                UploadTaskState::Running,
                UploadTaskState::Paused,
            ],
            UploadTaskState::Canceled,
        )
    }

    /// Registers an observer that receives a snapshot on every state change and chunk completion,
    /// mirroring `uploadTask.on('state_changed', ...)`.
    ///
    /// The returned closure unsubscribes the observer.
    pub fn on_state_changed<F>(&self, observer: F) -> impl FnOnce() + Send + 'static
    where
        F: FnMut(UploadTaskSnapshot) + Send + 'static,
    {
        let id = self.shared.add_observer(Box::new(observer));
        let shared = Arc::clone(&self.shared);
        move || shared.remove_observer(id)
    }
}

impl fmt::Debug for UploadTaskHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadTaskHandle")
            .field("path", &self.shared.reference.full_path())
            .field("state", &self.shared.state())
            .finish()
    }
}

/// Stateful helper that mirrors the Firebase Web SDK's resumable upload behaviour.
///
/// A task is created via [`StorageReference::upload_bytes_resumable`](crate::StorageReference::upload_bytes_resumable)
/// and can then be polled chunk-by-chunk (`upload_next`) or allowed to run to completion (`run_to_completion`).
/// Small payloads are uploaded with a single multipart request, whereas larger blobs utilise the resumable REST API.
///
/// Uploads can be observed and controlled while they run:
///
/// ```no_run
/// # use firebase_storage::StorageReference;
/// # async fn demo(reference: StorageReference) -> Result<(), Box<dyn std::error::Error>> {
/// let task = reference.upload_bytes_resumable(vec![0_u8; 4 * 1024 * 1024], None)?;
/// let handle = task.handle();
/// let unsubscribe = handle.on_state_changed(|snapshot| {
///     println!("{} of {} bytes", snapshot.bytes_transferred, snapshot.total_bytes);
/// });
/// // `handle` can be moved elsewhere to pause(), resume() or cancel() the upload.
/// let metadata = task.run_to_completion().await?;
/// unsubscribe();
/// println!("uploaded {:?}", metadata.name);
/// # Ok(())
/// # }
/// ```
pub struct UploadTask {
    reference: StorageReference,
    data: Vec<u8>,
    metadata: Option<UploadMetadata>,
    resumable: bool,
    chunk_multiplier: usize,
    cancel_notified: bool,
    pause_poll_interval: Duration,
    shared: Arc<UploadShared>,
}

impl UploadTask {
    pub(crate) fn new(reference: StorageReference, data: Vec<u8>, metadata: Option<UploadMetadata>) -> Self {
        let total_bytes = data.len() as u64;
        let resumable = total_bytes as usize > RESUMABLE_UPLOAD_CHUNK_SIZE;
        let shared = Arc::new(UploadShared::new(reference.clone(), total_bytes));
        Self {
            reference,
            data,
            metadata,
            resumable,
            chunk_multiplier: 1,
            cancel_notified: false,
            pause_poll_interval: DEFAULT_PAUSE_POLL_INTERVAL,
            shared,
        }
    }

    /// Returns a cloneable handle that can pause, resume, cancel and observe this task.
    pub fn handle(&self) -> UploadTaskHandle {
        UploadTaskHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Returns the total number of bytes that will be uploaded.
    pub fn total_bytes(&self) -> u64 {
        self.shared.total_bytes()
    }

    /// Returns the number of bytes that have been successfully uploaded so far.
    pub fn bytes_transferred(&self) -> u64 {
        self.shared.bytes_transferred()
    }

    /// Current task state.
    pub fn state(&self) -> UploadTaskState {
        self.shared.state()
    }

    /// Snapshot of the task's progress.
    pub fn snapshot(&self) -> UploadTaskSnapshot {
        self.shared.snapshot()
    }

    /// Last error reported by the task, if any.
    pub fn last_error(&self) -> Option<StorageError> {
        self.shared.error()
    }

    /// Resulting object metadata after a successful upload.
    pub fn metadata(&self) -> Option<ObjectMetadata> {
        self.shared.metadata()
    }

    /// The resumable session URL, if one has been established.
    pub fn upload_session_url(&self) -> Option<String> {
        self.shared.upload_url()
    }

    /// True when the payload is large enough to use the resumable protocol.
    pub fn is_resumable(&self) -> bool {
        self.resumable
    }

    /// Pauses the upload. See [`UploadTaskHandle::pause`].
    pub fn pause(&self) -> bool {
        self.handle().pause()
    }

    /// Resumes a paused upload. See [`UploadTaskHandle::resume`].
    pub fn resume(&self) -> bool {
        self.handle().resume()
    }

    /// Cancels the upload. See [`UploadTaskHandle::cancel`].
    pub fn cancel(&self) -> bool {
        self.handle().cancel()
    }

    /// Registers a state observer. See [`UploadTaskHandle::on_state_changed`].
    pub fn on_state_changed<F>(&self, observer: F) -> impl FnOnce() + Send + 'static
    where
        F: FnMut(UploadTaskSnapshot) + Send + 'static,
    {
        self.handle().on_state_changed(observer)
    }

    /// Overrides how often [`run_to_completion`](Self::run_to_completion) polls while paused.
    pub fn set_pause_poll_interval(&mut self, interval: Duration) {
        self.pause_poll_interval = interval;
    }

    /// Asks the server how many bytes of the resumable session it has stored and syncs the task to
    /// that offset.
    ///
    /// Returns the number of bytes the server acknowledged. Tasks that have not opened a resumable
    /// session yet report their local progress instead.
    pub async fn refresh_status(&mut self) -> StorageResult<u64> {
        let Some(upload_url) = self.shared.upload_url() else {
            return Ok(self.shared.bytes_transferred());
        };
        let storage = self.reference.storage();
        let request = get_resumable_upload_status_request(
            &storage,
            self.reference.location(),
            &upload_url,
            self.shared.total_bytes(),
        );
        let status = storage.run_upload_request(request).await?;
        self.shared.set_transferred(status.current);
        self.shared.emit();
        Ok(status.current)
    }

    /// Uploads the next chunk and invokes the provided progress callback.
    ///
    /// Returns `Ok(Some(metadata))` when the upload finishes and the remote metadata is available.
    /// A paused task performs no work and returns `Ok(None)`.
    pub async fn upload_next_with_progress<F>(&mut self, mut progress: F) -> StorageResult<Option<ObjectMetadata>>
    where
        F: FnMut(UploadProgress),
    {
        match self.shared.state() {
            UploadTaskState::Completed => {
                return Ok(self.shared.metadata());
            }
            UploadTaskState::Error => {
                return Err(self
                    .shared
                    .error()
                    .unwrap_or_else(|| internal_error("upload task failed")));
            }
            UploadTaskState::Canceled => {
                return Err(self.finish_cancellation().await);
            }
            UploadTaskState::Paused => return Ok(None),
            UploadTaskState::Pending | UploadTaskState::Running => {}
        }

        if !self.resumable {
            return self.upload_multipart(progress).await;
        }

        self.shared
            .transition(&[UploadTaskState::Pending], UploadTaskState::Running);

        if let Err(err) = self.ensure_resumable_session().await {
            return self.fail(err);
        }

        match self.shared.state() {
            UploadTaskState::Canceled => return Err(self.finish_cancellation().await),
            UploadTaskState::Paused => return Ok(None),
            _ => {}
        }

        let storage = self.reference.storage();
        let upload_url = self
            .shared
            .upload_url()
            .ok_or_else(|| internal_error("resumable session url missing"))?;
        let total_bytes = self.shared.total_bytes();
        let start_offset = self.shared.bytes_transferred();
        let chunk_size = self.current_chunk_size() as u64;
        let end_offset = cmp::min(total_bytes, start_offset + chunk_size);
        let finalize = end_offset == total_bytes;
        let chunk = self
            .data
            .get(start_offset as usize..end_offset as usize)
            .map(|slice| slice.to_vec())
            .unwrap_or_default();

        let request = continue_resumable_upload_request(
            &storage,
            self.reference.location(),
            &upload_url,
            start_offset,
            total_bytes,
            chunk,
            finalize,
        );
        let status = match storage.run_upload_request(request).await {
            Ok(status) => status,
            Err(err) => {
                self.reset_multiplier();
                if self.shared.state() == UploadTaskState::Canceled {
                    return Err(self.finish_cancellation().await);
                }
                return self.fail(err);
            }
        };

        // A cancel that arrived while the chunk was in flight wins: the session is discarded so the
        // object never materialises.
        if self.shared.state() == UploadTaskState::Canceled {
            return Err(self.finish_cancellation().await);
        }

        self.shared.set_transferred(status.current);
        progress(UploadProgress::new(status.current, total_bytes));

        if status.finalized {
            let metadata = status
                .metadata
                .ok_or_else(|| internal_error("resumable upload completed without metadata"))?;
            self.shared.complete(metadata.clone());
            Ok(Some(metadata))
        } else {
            self.shared.emit();
            self.bump_multiplier();
            Ok(None)
        }
    }

    /// Uploads the next chunk without emitting progress callbacks.
    pub async fn upload_next(&mut self) -> StorageResult<Option<ObjectMetadata>> {
        self.upload_next_with_progress(|_| {}).await
    }

    /// Runs the task to completion while notifying `progress` for each chunk.
    ///
    /// A paused task keeps waiting until it is resumed or cancelled; a cancelled task fails with
    /// `storage/canceled`.
    pub async fn run_to_completion_with_progress<F>(mut self, mut progress: F) -> StorageResult<ObjectMetadata>
    where
        F: FnMut(UploadProgress),
    {
        loop {
            match self.shared.state() {
                UploadTaskState::Paused => {
                    runtime::sleep(self.pause_poll_interval).await;
                    continue;
                }
                UploadTaskState::Canceled => {
                    return Err(self.finish_cancellation().await);
                }
                _ => {}
            }

            if let Some(metadata) = self.upload_next_with_progress(&mut progress).await? {
                return Ok(metadata);
            }
        }
    }

    /// Runs the task to completion without progress callbacks.
    pub async fn run_to_completion(self) -> StorageResult<ObjectMetadata> {
        self.run_to_completion_with_progress(|_| {}).await
    }

    async fn ensure_resumable_session(&mut self) -> StorageResult<()> {
        if !self.resumable || self.shared.upload_url().is_some() {
            return Ok(());
        }
        let storage = self.reference.storage();
        let request = create_resumable_upload_request(
            &storage,
            self.reference.location(),
            self.metadata.clone(),
            self.shared.total_bytes(),
        );
        let url = storage.run_upload_request(request).await?;
        self.shared.set_upload_url(url);
        Ok(())
    }

    async fn upload_multipart<F>(&mut self, mut progress: F) -> StorageResult<Option<ObjectMetadata>>
    where
        F: FnMut(UploadProgress),
    {
        if self.shared.state() == UploadTaskState::Completed {
            return Ok(self.shared.metadata());
        }

        self.shared
            .transition(&[UploadTaskState::Pending], UploadTaskState::Running);
        let storage = self.reference.storage();
        let request =
            multipart_upload_request(&storage, self.reference.location(), self.data.clone(), self.metadata.clone());

        match storage.run_upload_request(request).await {
            Ok(metadata) => {
                // Small payloads go up in one request, so a cancel can only be honoured before it
                // starts; once the server accepted the bytes the object exists.
                if self.shared.state() == UploadTaskState::Canceled {
                    return Err(self.finish_cancellation().await);
                }
                let total_bytes = self.shared.total_bytes();
                self.shared.complete(metadata.clone());
                progress(UploadProgress::new(total_bytes, total_bytes));
                Ok(Some(metadata))
            }
            Err(err) => self.fail(err),
        }
    }

    /// Discards the resumable session server-side (best effort) and reports `storage/canceled`.
    async fn finish_cancellation(&mut self) -> StorageError {
        let error = canceled();
        if !self.cancel_notified {
            self.cancel_notified = true;
            if let Some(upload_url) = self.shared.upload_url() {
                let storage = self.reference.storage();
                let request = cancel_resumable_upload_request(&storage, self.reference.location(), &upload_url);
                let _ = storage.run_upload_request(request).await;
            }
        }
        self.shared.record_cancellation(error.clone());
        error
    }

    fn current_chunk_size(&self) -> usize {
        cmp::min(RESUMABLE_UPLOAD_CHUNK_SIZE * self.chunk_multiplier, MAX_RESUMABLE_CHUNK_SIZE)
    }

    fn bump_multiplier(&mut self) {
        let next = self.chunk_multiplier * 2;
        if next * RESUMABLE_UPLOAD_CHUNK_SIZE <= MAX_RESUMABLE_CHUNK_SIZE {
            self.chunk_multiplier = next;
        }
    }

    fn reset_multiplier(&mut self) {
        self.chunk_multiplier = 1;
    }

    fn fail<T>(&mut self, error: StorageError) -> StorageResult<T> {
        self.shared.fail(error.clone());
        Err(error)
    }
}

impl fmt::Debug for UploadTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadTask")
            .field("path", &self.reference.full_path())
            .field("state", &self.shared.state())
            .field("bytes_transferred", &self.shared.bytes_transferred())
            .field("total_bytes", &self.shared.total_bytes())
            .field("resumable", &self.resumable)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::StorageErrorCode;
    use crate::service::FirebaseStorageImpl;
    use firebase_core::app::{initialize_app, FirebaseAppSettings, FirebaseOptions};

    fn unique_settings() -> FirebaseAppSettings {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        FirebaseAppSettings {
            name: Some(format!("storage-upload-{}", COUNTER.fetch_add(1, Ordering::SeqCst))),
            ..Default::default()
        }
    }

    async fn build_reference(path: &str) -> StorageReference {
        let options = FirebaseOptions {
            storage_bucket: Some("my-bucket".into()),
            ..Default::default()
        };
        let app = initialize_app(options, Some(unique_settings())).await.unwrap();
        let storage = FirebaseStorageImpl::new(app, None, None).unwrap();
        storage.root_reference().unwrap().child(path)
    }

    async fn build_task(size: usize) -> UploadTask {
        let reference = build_reference("uploads/blob.bin").await;
        UploadTask::new(reference, vec![7_u8; size], None)
    }

    #[test]
    fn task_states_map_to_the_web_sdk_strings() {
        assert_eq!(UploadTaskState::Running.as_str(), "running");
        assert_eq!(UploadTaskState::Paused.as_str(), "paused");
        assert_eq!(UploadTaskState::Completed.as_str(), "success");
        assert_eq!(UploadTaskState::Canceled.as_str(), "canceled");
        assert_eq!(UploadTaskState::Error.as_str(), "error");
        assert!(UploadTaskState::Completed.is_terminal());
        assert!(!UploadTaskState::Paused.is_terminal());
    }

    #[test]
    fn handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<UploadTaskHandle>();
    }

    #[tokio::test]
    async fn small_payloads_use_multipart_and_large_ones_resume() {
        let small = build_task(1024).await;
        assert!(!small.is_resumable());
        let large = build_task(RESUMABLE_UPLOAD_CHUNK_SIZE + 1).await;
        assert!(large.is_resumable());
        assert_eq!(large.total_bytes(), RESUMABLE_UPLOAD_CHUNK_SIZE as u64 + 1);
        assert_eq!(large.state(), UploadTaskState::Pending);
    }

    #[tokio::test]
    async fn pause_resume_and_cancel_follow_the_web_sdk_state_machine() {
        let task = build_task(4096).await;
        let handle = task.handle();

        assert_eq!(handle.state(), UploadTaskState::Pending);
        assert!(handle.pause(), "a pending task can be paused");
        assert_eq!(handle.state(), UploadTaskState::Paused);
        assert!(!handle.pause(), "pausing twice is a no-op");

        assert!(handle.resume());
        assert_eq!(handle.state(), UploadTaskState::Running);
        assert!(!handle.resume(), "resuming a running task is a no-op");

        assert!(handle.cancel());
        assert_eq!(handle.state(), UploadTaskState::Canceled);
        assert!(!handle.cancel(), "cancelling twice is a no-op");
        assert!(!handle.pause(), "a cancelled task cannot be paused");
        assert!(!handle.resume(), "a cancelled task cannot be resumed");
    }

    #[tokio::test]
    async fn observers_receive_snapshots_until_they_unsubscribe() {
        let task = build_task(4096).await;
        let handle = task.handle();
        let seen = Arc::new(Mutex::new(Vec::new()));

        let recorder = Arc::clone(&seen);
        let unsubscribe = handle.on_state_changed(move |snapshot| {
            lock(&recorder).push((snapshot.state, snapshot.bytes_transferred, snapshot.total_bytes));
        });

        handle.pause();
        handle.resume();
        unsubscribe();
        handle.cancel();

        let seen = lock(&seen).clone();
        assert_eq!(
            seen,
            vec![(UploadTaskState::Paused, 0, 4096), (UploadTaskState::Running, 0, 4096)],
            "the observer must stop receiving events after unsubscribing"
        );
    }

    #[tokio::test]
    async fn state_changes_made_inside_an_observer_are_delivered() {
        let task = build_task(4096).await;
        let handle = task.handle();
        let seen = Arc::new(Mutex::new(Vec::new()));

        let recorder = Arc::clone(&seen);
        let inner = handle.clone();
        let _unsubscribe = handle.on_state_changed(move |snapshot| {
            lock(&recorder).push(snapshot.state);
            if snapshot.state == UploadTaskState::Running {
                // Cancelling from inside a callback must not recurse, but must still be observed.
                inner.cancel();
            }
        });

        handle.pause();
        handle.resume();

        assert_eq!(handle.state(), UploadTaskState::Canceled);
        assert_eq!(
            lock(&seen).clone(),
            vec![
                UploadTaskState::Paused,
                UploadTaskState::Running,
                UploadTaskState::Canceled
            ]
        );
    }

    #[tokio::test]
    async fn a_cancelled_task_fails_with_storage_canceled() {
        let mut task = build_task(4096).await;
        assert!(task.cancel());

        let err = task.upload_next().await.expect_err("cancelled task must fail");
        assert_eq!(err.code, StorageErrorCode::Canceled);
        assert_eq!(err.code_str(), "storage/canceled");
        assert_eq!(task.state(), UploadTaskState::Canceled);
        assert_eq!(task.last_error().map(|err| err.code), Some(StorageErrorCode::Canceled));

        // The error is sticky: a second attempt reports the same failure.
        let err = task.upload_next().await.expect_err("still cancelled");
        assert_eq!(err.code, StorageErrorCode::Canceled);
    }

    #[tokio::test]
    async fn a_paused_task_does_no_work() {
        let mut task = build_task(4096).await;
        assert!(task.pause());
        assert!(
            task.upload_next()
                .await
                .expect("paused tasks report no progress")
                .is_none(),
            "a paused task must not upload"
        );
        assert_eq!(task.state(), UploadTaskState::Paused);
        assert_eq!(task.bytes_transferred(), 0);
    }

    #[tokio::test]
    async fn snapshots_describe_the_reference() {
        let task = build_task(4096).await;
        let snapshot = task.snapshot();
        assert_eq!(snapshot.total_bytes, 4096);
        assert_eq!(snapshot.bytes_transferred, 0);
        assert_eq!(snapshot.state, UploadTaskState::Pending);
        assert_eq!(snapshot.reference.full_path(), "uploads/blob.bin");
        assert_eq!(snapshot.progress(), UploadProgress::new(0, 4096));
        assert!(format!("{snapshot:?}").contains("uploads/blob.bin"));
    }
}
