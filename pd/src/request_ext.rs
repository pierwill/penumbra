use sha2::{Digest, Sha256};
use tendermint::abci::{
    request::{BeginBlock, CheckTx, DeliverTx, EndBlock, InitChain, Query},
    Request,
};
use tracing::error_span;

pub trait RequestExt {
    /// Create a [`tracing::Span`] for this request, including the request name
    /// and some relevant context (but not including the entire request data).
    fn create_span(&self) -> tracing::Span;
}

impl RequestExt for Request {
    fn create_span(&self) -> tracing::Span {
        // Create a parent "abci" span. All of these spans are at error level, so they're always recorded.
        let p = error_span!("abci");
        match self {
            Request::Info(_) => error_span!(parent: &p, "Info"),
            Request::Query(Query {
                path,
                height,
                prove,
                ..
            }) => {
                error_span!(parent: &p, "Query", ?path, ?height, prove)
            }
            Request::CheckTx(CheckTx { kind, tx }) => {
                error_span!(parent: &p, "CheckTx", ?kind, txid = ?hex::encode(&Sha256::digest(tx.as_ref())))
            }
            Request::BeginBlock(BeginBlock { hash, header, .. }) => {
                error_span!(parent: &p, "BeginBlock", height = ?header.height, hash = ?hex::encode(hash.as_ref()))
            }
            Request::DeliverTx(DeliverTx { tx }) => {
                error_span!(parent: &p, "DeliverTx", txid = ?hex::encode(&Sha256::digest(tx.as_ref())))
            }
            Request::EndBlock(EndBlock { height }) => error_span!(parent: &p, "EndBlock", ?height),
            Request::Commit => error_span!(parent: &p, "Commit"),
            Request::InitChain(InitChain { chain_id, .. }) => {
                error_span!(parent: &p, "InitChain", ?chain_id)
            }
            Request::Flush => error_span!(parent: &p, "Flush"),
            Request::Echo(_) => error_span!(parent: &p, "Echo"),
            Request::ListSnapshots => error_span!(parent: &p, "ListSnapshots"),
            Request::OfferSnapshot(_) => error_span!(parent: &p, "OfferSnapshot"),
            Request::LoadSnapshotChunk(_) => error_span!(parent: &p, "LoadSnapshotChunk"),
            Request::ApplySnapshotChunk(_) => error_span!(parent: &p, "ApplySnapshotChunk"),
        }
    }
}
