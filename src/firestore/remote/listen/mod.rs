//! The Firestore `Listen` streaming RPC, which powers `on_snapshot`.
//!
//! Everything else in this crate talks to Firestore over REST, but `Listen`
//! (`google.firestore.v1.Firestore/Listen`) is a bidirectional streaming RPC with no REST
//! equivalent, so snapshot listeners speak gRPC through [`tonic`]. The stream carries
//! [`WatchChange`]s, which the existing [`WatchChangeAggregator`](super::watch_change_aggregator)
//! folds into `RemoteEvent`s exactly as the JS SDK does.

pub(crate) mod convert;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Request, Status, Streaming};

use crate::firestore::api::query::QueryDefinition;
use crate::firestore::error::{internal_error, invalid_argument, unavailable, FirestoreError, FirestoreResult};
use crate::firestore::model::{DatabaseId, DocumentKey};
use crate::firestore::remote::proto::google::firestore::v1 as fs;
use crate::firestore::remote::serializer::JsonProtoSerializer;
use crate::firestore::remote::watch_change::{
    DocumentChange, DocumentDelete, DocumentRemove, ExistenceFilterChange, TargetChangeState, WatchChange,
    WatchTargetChange,
};
use crate::firestore::TokenProviderArc;

use convert::{document_from_proto, structured_query_to_proto, timestamp_from_proto};

/// Default production endpoint; emulators override it with a plain-HTTP address.
const FIRESTORE_GRPC_ENDPOINT: &str = "https://firestore.googleapis.com";

/// The single target every listener registers. Firestore allows many targets per stream; one
/// stream per listener keeps the bookkeeping simple and matches how short-lived listeners behave.
pub(crate) const LISTEN_TARGET_ID: i32 = 1;

/// What a listener watches: one document, or a query.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum ListenTarget {
    Document(DocumentKey),
    Query(QueryDefinition),
}

/// Opens `Listen` streams for one Firestore instance.
pub(crate) struct ListenTransport {
    endpoint: String,
    database_id: DatabaseId,
    serializer: JsonProtoSerializer,
    auth_provider: Option<TokenProviderArc>,
    app_check_provider: Option<TokenProviderArc>,
}

/// A live `Listen` stream. Dropping it closes the request channel, which ends the RPC.
pub(crate) struct ListenSession {
    #[allow(dead_code)]
    requests: mpsc::Sender<fs::ListenRequest>,
    responses: Streaming<fs::ListenResponse>,
}

impl ListenSession {
    /// Waits for the next message from the server.
    pub(crate) async fn next(&mut self) -> Result<Option<fs::ListenResponse>, Status> {
        self.responses.message().await
    }
}

impl ListenTransport {
    pub(crate) fn new(
        database_id: DatabaseId,
        emulator_host: Option<String>,
        auth_provider: Option<TokenProviderArc>,
        app_check_provider: Option<TokenProviderArc>,
    ) -> Self {
        let endpoint = match emulator_host {
            // Emulators speak h2c: no TLS, and any scheme the caller already supplied wins.
            Some(host) if host.starts_with("http://") || host.starts_with("https://") => host,
            Some(host) => format!("http://{host}"),
            None => FIRESTORE_GRPC_ENDPOINT.to_string(),
        };

        Self {
            serializer: JsonProtoSerializer::new(database_id.clone()),
            endpoint,
            database_id,
            auth_provider,
            app_check_provider,
        }
    }

    /// Resolves the emulator host the same way the REST connection does.
    pub(crate) fn emulator_host_from_env() -> Option<String> {
        std::env::var("FIRESTORE_EMULATOR_HOST").ok()
    }

    pub(crate) fn serializer(&self) -> &JsonProtoSerializer {
        &self.serializer
    }

    fn database_name(&self) -> String {
        format!(
            "projects/{}/databases/{}",
            self.database_id.project_id(),
            self.database_id.database()
        )
    }

    /// Opens a stream and registers `target`, resuming from `resume_token` when one is available.
    pub(crate) async fn open(
        &self,
        target: &ListenTarget,
        resume_token: Option<Vec<u8>>,
    ) -> FirestoreResult<ListenSession> {
        let channel = self.connect().await?;
        let mut client = fs::firestore_client::FirestoreClient::new(channel);

        let (sender, receiver) = mpsc::channel(8);
        sender
            .send(self.listen_request(target, resume_token)?)
            .await
            .map_err(|_| internal_error("listen request channel closed before the stream started"))?;

        let mut request = Request::new(ReceiverStream::new(receiver));
        self.attach_metadata(&mut request).await?;

        let responses = client
            .listen(request)
            .await
            .map_err(|status| map_status(&status))?
            .into_inner();

        Ok(ListenSession {
            requests: sender,
            responses,
        })
    }

    async fn connect(&self) -> FirestoreResult<Channel> {
        let mut endpoint = Endpoint::from_shared(self.endpoint.clone())
            .map_err(|err| invalid_argument(format!("invalid Firestore endpoint '{}': {err}", self.endpoint)))?
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_while_idle(true);

        if self.endpoint.starts_with("https://") {
            endpoint = endpoint
                .tls_config(ClientTlsConfig::new())
                .map_err(|err| internal_error(format!("failed to configure TLS for Firestore: {err}")))?;
        }

        endpoint
            .connect()
            .await
            .map_err(|err| unavailable(format!("failed to connect to Firestore at {}: {err}", self.endpoint)))
    }

    fn listen_request(
        &self,
        target: &ListenTarget,
        resume_token: Option<Vec<u8>>,
    ) -> FirestoreResult<fs::ListenRequest> {
        let database = self.database_name();
        let target_type = match target {
            ListenTarget::Document(key) => fs::target::TargetType::Documents(fs::target::DocumentsTarget {
                documents: vec![self.serializer.document_name(key)],
            }),
            ListenTarget::Query(definition) => fs::target::TargetType::Query(fs::target::QueryTarget {
                parent: format!("{database}/documents{}", parent_suffix(definition)),
                query_type: Some(fs::target::query_target::QueryType::StructuredQuery(structured_query_to_proto(
                    definition,
                )?)),
            }),
        };

        Ok(fs::ListenRequest {
            database,
            labels: HashMap::new(),
            request_options: None,
            target_change: Some(fs::listen_request::TargetChange::AddTarget(fs::Target {
                target_id: LISTEN_TARGET_ID,
                once: false,
                expected_count: None,
                // Resuming tells the server to replay only what changed since the token was issued.
                resume_type: resume_token.map(fs::target::ResumeType::ResumeToken),
                target_type: Some(target_type),
            })),
        })
    }

    /// Adds the routing headers Firestore needs plus the caller's credentials.
    async fn attach_metadata<T>(&self, request: &mut Request<T>) -> FirestoreResult<()> {
        let database = self.database_name();
        let metadata = request.metadata_mut();

        // Without this the backend (and the emulator) cannot tell which database the stream is for.
        // This is the only routing header Firestore wants: production compares `x-goog-request-params`
        // against the request's database name and rejects streams where the two are not identical,
        // so sending it as well buys nothing and can only go wrong.
        metadata.insert(
            "google-cloud-resource-prefix",
            parse_metadata(&database, "google-cloud-resource-prefix")?,
        );

        if let Some(provider) = &self.auth_provider {
            if let Some(token) = provider
                .get_token()
                .await
                .map_err(|err| internal_error(err.to_string()))?
            {
                metadata.insert("authorization", parse_metadata(&format!("Bearer {token}"), "authorization")?);
            }
        }

        if let Some(provider) = &self.app_check_provider {
            if let Some(token) = provider
                .get_token()
                .await
                .map_err(|err| internal_error(err.to_string()))?
            {
                metadata.insert("x-firebase-appcheck", parse_metadata(&token, "x-firebase-appcheck")?);
            }
        }

        Ok(())
    }
}

fn parse_metadata(value: &str, name: &str) -> FirestoreResult<MetadataValue<tonic::metadata::Ascii>> {
    value
        .parse()
        .map_err(|_| internal_error(format!("invalid value for the {name} header")))
}

/// A query target's parent is the collection's parent document (or the database root).
fn parent_suffix(definition: &QueryDefinition) -> String {
    let parent = definition.parent_path();
    if parent.is_empty() {
        String::new()
    } else {
        format!("/{}", parent.canonical_string())
    }
}

/// Maps a gRPC status onto the crate's error type.
pub(crate) fn map_status(status: &Status) -> FirestoreError {
    map_status_code(status.code() as i32, status.message())
}

pub(crate) fn map_status_code(code: i32, message: &str) -> FirestoreError {
    let message = if message.is_empty() {
        "Firestore listen stream error".to_string()
    } else {
        message.to_string()
    };
    // The numbers are gRPC status codes (google.rpc.Code).
    match code {
        3 => invalid_argument(message),
        4 => crate::firestore::error::deadline_exceeded(message),
        5 => crate::firestore::error::not_found(message),
        6 => crate::firestore::error::already_exists(message),
        7 => crate::firestore::error::permission_denied(message),
        8 => crate::firestore::error::resource_exhausted(message),
        // A query that needs a composite index arrives as FAILED_PRECONDITION.
        9 => crate::firestore::error::failed_precondition(message),
        10 => crate::firestore::error::aborted(message),
        14 => unavailable(message),
        16 => crate::firestore::error::unauthenticated(message),
        _ => internal_error(message),
    }
}

/// True for the status codes the JS SDK reconnects after; everything else is permanent.
pub(crate) fn is_retryable_status(status: &Status) -> bool {
    matches!(
        status.code(),
        tonic::Code::Cancelled
            | tonic::Code::Unknown
            | tonic::Code::DeadlineExceeded
            | tonic::Code::ResourceExhausted
            | tonic::Code::Internal
            | tonic::Code::Unavailable
            | tonic::Code::Unauthenticated
    )
}

/// Translates one `ListenResponse` into the watch model shared with the REST/JSON path.
pub(crate) fn decode_listen_response(
    serializer: &JsonProtoSerializer,
    response: &fs::ListenResponse,
) -> FirestoreResult<Option<WatchChange>> {
    let Some(response_type) = response.response_type.as_ref() else {
        return Ok(None);
    };

    let change = match response_type {
        fs::listen_response::ResponseType::TargetChange(change) => {
            let state = match fs::target_change::TargetChangeType::try_from(change.target_change_type) {
                Ok(fs::target_change::TargetChangeType::NoChange) => TargetChangeState::NoChange,
                Ok(fs::target_change::TargetChangeType::Add) => TargetChangeState::Add,
                Ok(fs::target_change::TargetChangeType::Remove) => TargetChangeState::Remove,
                Ok(fs::target_change::TargetChangeType::Current) => TargetChangeState::Current,
                Ok(fs::target_change::TargetChangeType::Reset) => TargetChangeState::Reset,
                Err(_) => {
                    return Err(internal_error(format!(
                        "unknown target change type {}",
                        change.target_change_type
                    )))
                }
            };

            WatchChange::TargetChange(WatchTargetChange {
                state,
                target_ids: change.target_ids.clone(),
                resume_token: (!change.resume_token.is_empty()).then(|| change.resume_token.to_vec()),
                read_time: change.read_time.as_ref().map(timestamp_from_proto),
                cause: change
                    .cause
                    .as_ref()
                    .map(|status| map_status_code(status.code, &status.message)),
            })
        }
        fs::listen_response::ResponseType::DocumentChange(change) => {
            let Some(document) = change.document.as_ref() else {
                return Err(internal_error("document change without a document"));
            };
            let watch_document = document_from_proto(serializer, document)?;
            WatchChange::DocumentChange(DocumentChange {
                updated_target_ids: change.target_ids.clone(),
                removed_target_ids: change.removed_target_ids.clone(),
                key: watch_document.key.clone(),
                document: Some(watch_document),
            })
        }
        fs::listen_response::ResponseType::DocumentDelete(delete) => WatchChange::DocumentDelete(DocumentDelete {
            key: serializer.document_key_from_name(&delete.document)?,
            read_time: delete.read_time.as_ref().map(timestamp_from_proto),
            removed_target_ids: delete.removed_target_ids.clone(),
        }),
        fs::listen_response::ResponseType::DocumentRemove(remove) => WatchChange::DocumentRemove(DocumentRemove {
            key: serializer.document_key_from_name(&remove.document)?,
            read_time: remove.read_time.as_ref().map(timestamp_from_proto),
            removed_target_ids: remove.removed_target_ids.clone(),
        }),
        fs::listen_response::ResponseType::Filter(filter) => WatchChange::ExistenceFilter(ExistenceFilterChange {
            target_id: filter.target_id,
            count: filter.count,
        }),
    };

    Ok(Some(change))
}

/// Shared handle so a listener task can be told to stop, and can wake up as soon as it is.
#[derive(Clone, Default)]
pub(crate) struct ListenCancellation {
    flag: Arc<std::sync::atomic::AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl ListenCancellation {
    pub(crate) fn cancel(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Resolves once [`cancel`](Self::cancel) has been called.
    pub(crate) async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firestore::model::DatabaseId;

    fn serializer() -> JsonProtoSerializer {
        JsonProtoSerializer::new(DatabaseId::new("demo-project", "(default)"))
    }

    fn document_name(path: &str) -> String {
        format!("projects/demo-project/databases/(default)/documents/{path}")
    }

    #[test]
    fn target_changes_decode_with_their_resume_token() {
        let response = fs::ListenResponse {
            response_type: Some(fs::listen_response::ResponseType::TargetChange(fs::TargetChange {
                target_change_type: fs::target_change::TargetChangeType::Current as i32,
                target_ids: vec![LISTEN_TARGET_ID],
                cause: None,
                resume_token: vec![7, 8, 9].into(),
                read_time: Some(prost_types::Timestamp { seconds: 5, nanos: 6 }),
            })),
        };

        let change = decode_listen_response(&serializer(), &response)
            .expect("decode")
            .expect("change");
        match change {
            WatchChange::TargetChange(target_change) => {
                assert_eq!(target_change.state, TargetChangeState::Current);
                assert_eq!(target_change.target_ids, vec![LISTEN_TARGET_ID]);
                assert_eq!(target_change.resume_token, Some(vec![7, 8, 9]));
                assert_eq!(target_change.read_time, Some(crate::firestore::model::Timestamp::new(5, 6)));
                assert!(target_change.cause.is_none());
            }
            other => panic!("expected a target change, got {other:?}"),
        }
    }

    #[test]
    fn a_rejected_target_keeps_the_servers_status_code() {
        let response = fs::ListenResponse {
            response_type: Some(fs::listen_response::ResponseType::TargetChange(fs::TargetChange {
                target_change_type: fs::target_change::TargetChangeType::Remove as i32,
                target_ids: vec![LISTEN_TARGET_ID],
                cause: Some(crate::firestore::remote::proto::google::rpc::Status {
                    code: 7,
                    message: "denied by the rules".to_string(),
                    details: Vec::new(),
                }),
                resume_token: Vec::new().into(),
                read_time: None,
            })),
        };

        let change = decode_listen_response(&serializer(), &response)
            .expect("decode")
            .expect("change");
        match change {
            WatchChange::TargetChange(target_change) => {
                let cause = target_change.cause.expect("cause");
                assert_eq!(cause.code_str(), "firestore/permission-denied");
                assert!(cause.to_string().contains("denied by the rules"));
            }
            other => panic!("expected a target change, got {other:?}"),
        }
    }

    #[test]
    fn document_changes_and_deletes_decode() {
        let serializer = serializer();
        let change = decode_listen_response(
            &serializer,
            &fs::ListenResponse {
                response_type: Some(fs::listen_response::ResponseType::DocumentChange(fs::DocumentChange {
                    document: Some(fs::Document {
                        name: document_name("rooms/lobby"),
                        fields: Default::default(),
                        create_time: None,
                        update_time: None,
                    }),
                    target_ids: vec![LISTEN_TARGET_ID],
                    removed_target_ids: Vec::new(),
                })),
            },
        )
        .expect("decode")
        .expect("change");
        match change {
            WatchChange::DocumentChange(change) => {
                assert_eq!(change.key.path().canonical_string(), "rooms/lobby");
                assert_eq!(change.updated_target_ids, vec![LISTEN_TARGET_ID]);
            }
            other => panic!("expected a document change, got {other:?}"),
        }

        let deleted = decode_listen_response(
            &serializer,
            &fs::ListenResponse {
                response_type: Some(fs::listen_response::ResponseType::DocumentDelete(fs::DocumentDelete {
                    document: document_name("rooms/lobby"),
                    removed_target_ids: vec![LISTEN_TARGET_ID],
                    read_time: None,
                })),
            },
        )
        .expect("decode")
        .expect("change");
        assert!(matches!(deleted, WatchChange::DocumentDelete(_)));
    }

    #[test]
    fn retryable_statuses_match_the_js_sdk() {
        assert!(is_retryable_status(&Status::unavailable("try again")));
        assert!(is_retryable_status(&Status::internal("hiccup")));
        assert!(!is_retryable_status(&Status::permission_denied("denied")));
        assert!(!is_retryable_status(&Status::failed_precondition("index missing")));
    }

    #[test]
    fn emulator_hosts_are_reached_over_plain_http() {
        let transport = ListenTransport::new(
            DatabaseId::new("demo-project", "(default)"),
            Some("127.0.0.1:8080".to_string()),
            None,
            None,
        );
        assert_eq!(transport.endpoint, "http://127.0.0.1:8080");

        let production = ListenTransport::new(DatabaseId::new("demo-project", "(default)"), None, None, None);
        assert_eq!(production.endpoint, FIRESTORE_GRPC_ENDPOINT);
    }

    #[test]
    fn a_cancelled_listener_reports_itself_as_cancelled() {
        let cancellation = ListenCancellation::default();
        assert!(!cancellation.is_cancelled());
        cancellation.cancel();
        assert!(cancellation.is_cancelled());
    }
}
