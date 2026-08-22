//! Backend boundary shared by standalone JCode and managed Automonique mode.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use automonique_platform_client::{ClientError, PlatformClient, PlatformTransport, UnixTransport};
use automonique_protocol::platform::{
    Capabilities, PlatformRequest, PlatformResponse, SnapshotRequest,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendMode {
    Standalone,
    Managed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendError {
    Unavailable,
    Protocol,
    ExhaustedFake,
}

impl From<ClientError> for BackendError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Io => Self::Unavailable,
            ClientError::Protocol | ClientError::Correlation | ClientError::ResponseTooLarge => {
                Self::Protocol
            }
        }
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
        assert_eq!(backend.capabilities().await, Ok(Capabilities::platform_v1()));
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
