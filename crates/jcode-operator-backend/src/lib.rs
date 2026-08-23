//! Backend boundary shared by standalone JCode and managed Automonique mode.

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use automonique_platform_client::{ClientError, PlatformClient, PlatformTransport, UnixTransport};
use automonique_protocol::platform::{
    ActionReceipt, AttachRequest, Attachment, Capabilities, ClaimControlRequest, ClientId,
    ControlLease, DetachRequest, ExecuteRequest, GetReceiptRequest, IdempotencyKey,
    ListSessionsRequest, PlatformAction, PlatformCursor, PlatformRequest, PlatformResponse,
    ReceiptOutcome, ReleaseControlRequest, ResourceAuthority, ResourceCoordinate, ResourceKind,
    ResourceRecord, SessionRecord, SnapshotRequest, SubscribeRequest, Subscription,
};

pub use automonique_protocol::codec as platform_codec;
pub use automonique_protocol::platform as platform_contract;
pub use automonique_protocol::platform_api;
pub use automonique_protocol::primitives as platform_primitives;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendMode {
    Standalone,
    Managed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendError {
    Unavailable,
    Protocol,
    Refused {
        outcome: ReceiptOutcome,
        explanation: String,
    },
    ExhaustedFake,
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("operator backend unavailable"),
            Self::Protocol => formatter.write_str("operator backend protocol refused"),
            Self::Refused {
                outcome,
                explanation,
            } => write!(
                formatter,
                "operator action {}: {explanation}",
                outcome.as_str()
            ),
            Self::ExhaustedFake => formatter.write_str("fake operator backend exhausted"),
        }
    }
}

impl std::error::Error for BackendError {}

impl From<ClientError> for BackendError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Io
            | ClientError::Endpoint
            | ClientError::Unauthorized
            | ClientError::UnexpectedStatus => Self::Unavailable,
            ClientError::Protocol
            | ClientError::Correlation
            | ClientError::ResponseTooLarge
            | ClientError::UnexpectedContentType => Self::Protocol,
        }
    }
}

fn refused(outcome: ReceiptOutcome, explanation: impl ToString) -> BackendError {
    BackendError::Refused {
        outcome,
        explanation: explanation.to_string(),
    }
}

/// The complete semantic boundary the TUI may use for operator state.
///
/// Managed implementations have no provider handle: every mutation is a
/// `PlatformRequest::Execute` and therefore receives a durable receipt.
#[async_trait]
pub trait OperatorBackend: Send + Sync {
    fn mode(&self) -> BackendMode;
    async fn request(&self, request: PlatformRequest) -> Result<PlatformResponse, BackendError>;

    async fn capabilities(&self) -> Result<Capabilities, BackendError> {
        match self.request(PlatformRequest::Capabilities).await? {
            PlatformResponse::Capabilities(value) => Ok(value),
            _ => Err(BackendError::Protocol),
        }
    }

    async fn snapshot(&self) -> Result<PlatformResponse, BackendError> {
        self.request(PlatformRequest::Snapshot(
            SnapshotRequest::new(Vec::new()).map_err(|_| BackendError::Protocol)?,
        ))
        .await
    }

    async fn overview(
        &self,
        authority: ResourceAuthority,
    ) -> Result<OperatorOverview, BackendError> {
        let capabilities = self.capabilities().await?;
        let snapshot = match self.snapshot().await? {
            PlatformResponse::Snapshot(value) => value,
            PlatformResponse::Refused {
                outcome,
                explanation,
            } => return Err(refused(outcome, explanation)),
            _ => return Err(BackendError::Protocol),
        };
        let sessions = if capabilities
            .methods
            .iter()
            .any(|method| method.as_str() == "list_sessions")
        {
            match self
                .request(PlatformRequest::ListSessions(ListSessionsRequest {
                    authority,
                    cursor: None,
                }))
                .await?
            {
                PlatformResponse::Sessions(value) => value.sessions,
                PlatformResponse::Refused { .. } => Vec::new(),
                _ => return Err(BackendError::Protocol),
            }
        } else {
            Vec::new()
        };
        let mut actions = Vec::new();
        for record in snapshot.resources.iter().filter(|record| {
            record.resource.authority == ResourceAuthority::Automonique
                && record.resource.kind == ResourceKind::Client
                && record.freshness.state.as_str() == "fresh"
        }) {
            let Some(value) = record.resource.id.as_str().strip_prefix("platform-action-") else {
                continue;
            };
            actions.push(PlatformAction::parse(value).map_err(|_| BackendError::Protocol)?);
        }
        Ok(OperatorOverview {
            capabilities,
            actions,
            resources: snapshot.resources,
            sessions,
            cursor: snapshot.cursor,
        })
    }

    async fn attach(
        &self,
        session: ResourceCoordinate,
        client: ClientId,
    ) -> Result<Attachment, BackendError> {
        match self
            .request(PlatformRequest::Attach(AttachRequest { session, client }))
            .await?
        {
            PlatformResponse::Attached(value) => Ok(value),
            PlatformResponse::Refused {
                outcome,
                explanation,
            } => Err(refused(outcome, explanation)),
            _ => Err(BackendError::Protocol),
        }
    }

    async fn claim_control(
        &self,
        session: ResourceCoordinate,
        client: ClientId,
        idempotency_key: IdempotencyKey,
    ) -> Result<ControlLease, BackendError> {
        match self
            .request(PlatformRequest::ClaimControl(ClaimControlRequest {
                session,
                client,
                idempotency_key,
            }))
            .await?
        {
            PlatformResponse::ControlClaimed(value) => Ok(value),
            PlatformResponse::Refused {
                outcome,
                explanation,
            } => Err(refused(outcome, explanation)),
            _ => Err(BackendError::Protocol),
        }
    }

    async fn subscribe(
        &self,
        cursor: Option<PlatformCursor>,
    ) -> Result<Subscription, BackendError> {
        match self
            .request(PlatformRequest::Subscribe(SubscribeRequest { cursor }))
            .await?
        {
            PlatformResponse::Subscription(value) => Ok(value),
            PlatformResponse::Refused {
                outcome,
                explanation,
            } => Err(refused(outcome, explanation)),
            _ => Err(BackendError::Protocol),
        }
    }

    async fn execute(&self, request: ExecuteRequest) -> Result<ActionReceipt, BackendError> {
        match self.request(PlatformRequest::Execute(request)).await? {
            PlatformResponse::Receipt(value) => Ok(value),
            PlatformResponse::Refused {
                outcome,
                explanation,
            } => Err(refused(outcome, explanation)),
            _ => Err(BackendError::Protocol),
        }
    }

    async fn receipt(&self, request: GetReceiptRequest) -> Result<ActionReceipt, BackendError> {
        match self.request(PlatformRequest::GetReceipt(request)).await? {
            PlatformResponse::Receipt(value) => Ok(value),
            PlatformResponse::Refused {
                outcome,
                explanation,
            } => Err(refused(outcome, explanation)),
            _ => Err(BackendError::Protocol),
        }
    }

    async fn detach(
        &self,
        session: ResourceCoordinate,
        client: ClientId,
    ) -> Result<(), BackendError> {
        match self
            .request(PlatformRequest::Detach(DetachRequest {
                session: session.clone(),
                client: client.clone(),
            }))
            .await?
        {
            PlatformResponse::Detached {
                session: detached,
                client: detached_client,
            } if detached == session && detached_client == client => Ok(()),
            PlatformResponse::Refused {
                outcome,
                explanation,
            } => Err(refused(outcome, explanation)),
            _ => Err(BackendError::Protocol),
        }
    }

    async fn release_control(
        &self,
        lease: ControlLease,
        idempotency_key: IdempotencyKey,
    ) -> Result<(), BackendError> {
        let session = lease.session.clone();
        let client = lease.client.clone();
        let lease_id = lease.id.clone();
        match self
            .request(PlatformRequest::ReleaseControl(ReleaseControlRequest {
                session: session.clone(),
                client: client.clone(),
                lease: lease_id.clone(),
                idempotency_key,
            }))
            .await?
        {
            PlatformResponse::ControlReleased {
                session: released,
                client: released_client,
                lease: released_lease,
            } if released == session && released_client == client && released_lease == lease_id => {
                Ok(())
            }
            PlatformResponse::Refused {
                outcome,
                explanation,
            } => Err(refused(outcome, explanation)),
            _ => Err(BackendError::Protocol),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperatorOverview {
    pub capabilities: Capabilities,
    /// Exact action vocabulary projected by the serving authority as v1
    /// resources. It is intentionally separate from generic method support.
    pub actions: Vec<PlatformAction>,
    pub resources: Vec<ResourceRecord>,
    pub sessions: Vec<SessionRecord>,
    pub cursor: PlatformCursor,
}

/// Managed backend. The only held authority is a platform client transport.
pub struct AutomoniqueBackend<T = UnixTransport> {
    client: Arc<Mutex<PlatformClient<T>>>,
}

impl AutomoniqueBackend<UnixTransport> {
    #[must_use]
    pub fn local(socket: impl Into<PathBuf>) -> Self {
        Self::new(UnixTransport::new(socket))
    }
}

impl<T: PlatformTransport> AutomoniqueBackend<T> {
    #[must_use]
    pub fn new(transport: T) -> Self {
        Self {
            client: Arc::new(Mutex::new(PlatformClient::new(transport))),
        }
    }
}

#[async_trait]
impl<T> OperatorBackend for AutomoniqueBackend<T>
where
    T: PlatformTransport + Send + 'static,
{
    fn mode(&self) -> BackendMode {
        BackendMode::Managed
    }

    async fn request(&self, request: PlatformRequest) -> Result<PlatformResponse, BackendError> {
        let client = Arc::clone(&self.client);
        tokio::task::spawn_blocking(move || {
            let mut client = client.lock().map_err(|_| BackendError::Unavailable)?;
            client.request(request).map_err(BackendError::from)
        })
        .await
        .map_err(|_| BackendError::Unavailable)?
    }
}

/// Deterministic backend for headless rendering and interaction tests.
pub struct FakeBackend {
    mode: BackendMode,
    responses: Mutex<VecDeque<Result<PlatformResponse, BackendError>>>,
    requests: Mutex<Vec<PlatformRequest>>,
}

impl FakeBackend {
    #[must_use]
    pub fn new(
        mode: BackendMode,
        responses: impl IntoIterator<Item = Result<PlatformResponse, BackendError>>,
    ) -> Self {
        Self {
            mode,
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn requests(&self) -> Result<Vec<PlatformRequest>, BackendError> {
        self.requests
            .lock()
            .map(|requests| requests.clone())
            .map_err(|_| BackendError::Unavailable)
    }
}

#[async_trait]
impl OperatorBackend for FakeBackend {
    fn mode(&self) -> BackendMode {
        self.mode
    }

    async fn request(&self, request: PlatformRequest) -> Result<PlatformResponse, BackendError> {
        self.requests
            .lock()
            .map_err(|_| BackendError::Unavailable)?
            .push(request);
        self.responses
            .lock()
            .map_err(|_| BackendError::Unavailable)?
            .pop_front()
            .ok_or(BackendError::ExhaustedFake)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_drives_capabilities_and_snapshot_without_network_or_provider() {
        let backend = FakeBackend::new(
            BackendMode::Managed,
            [
                Ok(PlatformResponse::Capabilities(Capabilities::platform_v1())),
                Err(BackendError::Unavailable),
            ],
        );
        assert_eq!(
            backend.capabilities().await,
            Ok(Capabilities::platform_v1())
        );
        assert_eq!(backend.snapshot().await, Err(BackendError::Unavailable));
        assert_eq!(backend.requests().expect("requests").len(), 2);
    }

    #[test]
    fn managed_backend_holds_only_the_platform_transport() {
        struct RefusingTransport;
        impl PlatformTransport for RefusingTransport {
            fn request(
                &mut self,
                _request_id: automonique_protocol::codec::RequestId,
                _request: PlatformRequest,
            ) -> Result<PlatformResponse, ClientError> {
                Err(ClientError::Io)
            }
        }
        let backend = AutomoniqueBackend::new(RefusingTransport);
        assert_eq!(backend.mode(), BackendMode::Managed);
    }
}
