//! Generation-scoped `AuthorizationQuery`/`AuthorizationDecision`
//! request/response routing (Issue #38; `m1-worker-data-plane-control-
//! contract.md` "Authorization query / decision", "Connection generations
//! and correlation").
//!
//! One connection task (`super::client::run_client_loop`) owns serialized
//! socket I/O for the whole connection lifetime; this module only provides
//! the narrow request/oneshot-correlation channel callers use to submit a
//! query and await its authoritative decision, never a general-purpose RPC
//! framework. A fresh channel is published for every successful handshake
//! (a fresh connection generation) — a caller holding a reference from a
//! prior generation observes [`QueryError::NotConnected`] the instant that
//! generation ends, never a response routed from the wrong generation.

use bamep_worker_protocol::{AuthorizationDecisionMessage, AuthorizationQueryMessage};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    /// No live, handshaken `bamepd` connection is currently available.
    /// Worker must treat this identically to a denial — it must never
    /// fabricate an `AuthorizationDecision` locally
    /// (`m1-worker-data-plane-control-contract.md` "Failure semantics": "UDS
    /// unavailable ... fail-closed").
    #[error("no live bamepd control connection is currently available")]
    NotConnected,
    /// The connection was lost (or the connection task ended) while this
    /// exact query was outstanding. Distinct from `NotConnected` only for
    /// diagnostics — both are fail-closed, never a fabricated decision.
    #[error("the bamepd control connection was lost while this query was outstanding")]
    Disconnected,
}

/// One outstanding query the connection task must send and correlate a reply
/// to. Public only because it appears in [`AuthorizationPublisher`]'s type
/// signature — external callers never construct this directly; use
/// [`AuthorizationClient::query`] instead.
pub struct PendingQuery {
    pub message: AuthorizationQueryMessage,
    pub reply: oneshot::Sender<AuthorizationDecisionMessage>,
}

type Publisher = watch::Sender<Option<mpsc::Sender<PendingQuery>>>;

/// Handle callers (present and future HTTP data-plane request handlers) use
/// to submit an `AuthorizationQuery` against whatever connection generation
/// is currently live. Cheap to clone; every clone observes the same
/// currently-published generation.
#[derive(Clone)]
pub struct AuthorizationClient {
    current: watch::Receiver<Option<mpsc::Sender<PendingQuery>>>,
}

impl AuthorizationClient {
    /// Sends `message` over the current live control connection and awaits
    /// its authoritative `AuthorizationDecision`. Fails closed — returns
    /// `Err`, never a fabricated decision — when no connection is currently
    /// `Ready`, or when the connection is lost while this exact query is
    /// outstanding (`m1-worker-data-plane-control-contract.md` "Disconnect
    /// with a request in flight: the outstanding request is treated as
    /// failed/uncertain, never as success").
    pub async fn query(
        &self,
        message: AuthorizationQueryMessage,
    ) -> Result<AuthorizationDecisionMessage, QueryError> {
        let sender = self.current.borrow().clone();
        let Some(sender) = sender else {
            return Err(QueryError::NotConnected);
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        if sender
            .send(PendingQuery {
                message,
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            // The connection task's receiver end has already been dropped —
            // the connection ended between the caller reading `current` and
            // this send.
            return Err(QueryError::NotConnected);
        }
        // A dropped `reply` sender (the connection task ended, or this exact
        // query's correlation could not be completed) surfaces as `Err`
        // here, never a fabricated `Ok`.
        reply_rx.await.map_err(|_| QueryError::Disconnected)
    }
}

/// Constructs the publisher/subscriber pair: `run_client_loop` uses the
/// returned [`AuthorizationPublisher`] to announce a fresh per-generation
/// request channel (or `None` while disconnected); [`AuthorizationClient`] is
/// the caller-facing handle the composition root hands to whatever consumes
/// authorization decisions.
pub fn channel() -> (AuthorizationPublisher, AuthorizationClient) {
    let (tx, rx) = watch::channel(None);
    (tx, AuthorizationClient { current: rx })
}

/// The `run_client_loop`-facing half of [`channel`] — opaque outside this
/// crate beyond being threaded straight into `run_client_loop`.
pub type AuthorizationPublisher = Publisher;

#[cfg(test)]
mod tests {
    use super::*;
    use bamep_worker_protocol::{AuthorizationOperation, WireTransferDirection};
    use uuid::Uuid;

    fn sample_query() -> AuthorizationQueryMessage {
        AuthorizationQueryMessage::new(
            "token",
            AuthorizationOperation::ResumeDiscovery,
            Uuid::new_v4(),
            Uuid::new_v4(),
            WireTransferDirection::AgentToServer,
            None,
            "proof-id",
            1,
            "signature",
        )
    }

    #[tokio::test]
    async fn a_query_with_no_published_connection_fails_not_connected() {
        let (_publisher, client) = channel();
        assert!(matches!(
            client.query(sample_query()).await,
            Err(QueryError::NotConnected)
        ));
    }

    #[tokio::test]
    async fn a_query_is_delivered_to_the_published_channel_and_correlates_its_reply() {
        let (publisher, client) = channel();
        let (tx, mut rx) = mpsc::channel(4);
        publisher.send(Some(tx)).unwrap();

        let query = sample_query();
        let sent_id = query.envelope.message_id;
        let handle = tokio::spawn(async move { client.query(query).await });

        let pending = rx
            .recv()
            .await
            .expect("connection task receives the pending query");
        assert_eq!(pending.message.envelope.message_id, sent_id);
        let decision = AuthorizationDecisionMessage::denied(sent_id);
        pending.reply.send(decision.clone()).unwrap();

        let result = handle.await.unwrap().expect("query succeeds");
        assert_eq!(result.body.in_reply_to, decision.body.in_reply_to);
    }

    #[tokio::test]
    async fn dropping_the_reply_sender_surfaces_as_disconnected() {
        let (publisher, client) = channel();
        let (tx, mut rx) = mpsc::channel(4);
        publisher.send(Some(tx)).unwrap();

        let handle = tokio::spawn(async move { client.query(sample_query()).await });
        let pending = rx
            .recv()
            .await
            .expect("connection task receives the pending query");
        drop(pending.reply);

        assert!(matches!(
            handle.await.unwrap(),
            Err(QueryError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn publishing_none_makes_subsequent_queries_fail_immediately() {
        let (publisher, client) = channel();
        let (tx, _rx) = mpsc::channel(4);
        publisher.send(Some(tx)).unwrap();
        publisher.send(None).unwrap();

        assert!(matches!(
            client.query(sample_query()).await,
            Err(QueryError::NotConnected)
        ));
    }
}
