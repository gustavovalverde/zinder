//! Private authenticated command surface for the running wallet owner.

use tokio::sync::{mpsc, oneshot};
use tonic::{Request, Response, Status, service::interceptor::InterceptedService};
use zinder_proto::v1::ingest::{
    CreateStateBundleCaptureRequest, CreateStateBundleCaptureResponse,
    projector_control_server::{ProjectorControl, ProjectorControlServer},
};
use zinder_runtime::{BearerToken, BearerTokenServerInterceptor};

/// Bounded owner-command channel capacity.
///
/// Capture is serialized by the sole
/// wallet primary, so callers receive resource exhaustion instead of piling up
/// unbounded work while a cold checkpoint is in progress.
pub(crate) const PROJECTOR_CONTROL_COMMAND_CAPACITY: usize = 1;

/// One command delegated to the wallet-owning following loop.
pub(crate) enum ProjectorControlCommand {
    /// Captures one coherent canonical-and-wallet state bundle.
    CreateStateBundleCapture {
        /// Opaque operator-selected candidate identifier.
        candidate_id: String,
        /// Reply sent only after manifest-last publication succeeds.
        reply: oneshot::Sender<Result<CreateStateBundleCaptureResponse, Status>>,
    },
}

/// Sender held by the authenticated gRPC adapter.
#[derive(Clone)]
pub(crate) struct ProjectorControlHandle {
    sender: mpsc::Sender<ProjectorControlCommand>,
}

impl ProjectorControlHandle {
    async fn create_state_bundle_capture(
        &self,
        candidate_id: String,
    ) -> Result<CreateStateBundleCaptureResponse, Status> {
        let (reply, receiver) = oneshot::channel();
        self.sender
            .try_send(ProjectorControlCommand::CreateStateBundleCapture {
                candidate_id,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Status::resource_exhausted(
                    "projector owner is already capturing a state bundle",
                ),
                mpsc::error::TrySendError::Closed(_) => {
                    Status::unavailable("projector wallet owner is not available for capture")
                }
            })?;
        receiver.await.map_err(|_| {
            Status::unavailable("projector wallet owner stopped before capture completed")
        })?
    }
}

/// Creates the one private command channel consumed by the wallet owner.
pub(crate) fn projector_control_channel() -> (
    ProjectorControlHandle,
    mpsc::Receiver<ProjectorControlCommand>,
) {
    let (sender, receiver) = mpsc::channel(PROJECTOR_CONTROL_COMMAND_CAPACITY);
    (ProjectorControlHandle { sender }, receiver)
}

/// Authenticated adapter that never opens or owns the wallet store.
#[derive(Clone)]
pub(crate) struct ProjectorControlGrpcAdapter {
    handle: ProjectorControlHandle,
    bearer_token: BearerToken,
}

impl ProjectorControlGrpcAdapter {
    /// Creates an adapter whose every method requires the dedicated token.
    #[must_use]
    pub(crate) fn new(handle: ProjectorControlHandle, bearer_token: BearerToken) -> Self {
        Self {
            handle,
            bearer_token,
        }
    }

    /// Builds the authenticated bounded tonic service.
    #[must_use]
    pub(crate) fn into_server(
        self,
    ) -> InterceptedService<ProjectorControlServer<Self>, BearerTokenServerInterceptor> {
        let interceptor = BearerTokenServerInterceptor::new(Some(self.bearer_token.clone()));
        let server = ProjectorControlServer::new(self)
            .max_decoding_message_size(zinder_runtime::MAX_DECODING_MESSAGE_BYTES);
        InterceptedService::new(server, interceptor)
    }
}

#[tonic::async_trait]
impl ProjectorControl for ProjectorControlGrpcAdapter {
    async fn create_state_bundle_capture(
        &self,
        request: Request<CreateStateBundleCaptureRequest>,
    ) -> Result<Response<CreateStateBundleCaptureResponse>, Status> {
        self.handle
            .create_state_bundle_capture(request.into_inner().candidate_id)
            .await
            .map(Response::new)
    }
}
