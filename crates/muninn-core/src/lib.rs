pub mod auth;
pub mod chunk;
pub mod expand;
pub mod gate;
mod http;
pub mod index;
pub mod lifecycle;
mod lifecycle_util;
pub mod memory;
pub mod pg;
pub mod rank;
pub mod recall_cache;
pub mod recall_latency;
pub mod scip;
pub mod symbols;

pub use chunk::{Chunk, chunk_file};
pub use expand::{ExpandMode, Expander, Expansion, dense_text, heuristic_expand, lexical_query};
pub use gate::{EMBED_GATE, Lane, RERANK_GATE, with_lane};
pub use http::{EMBED_BATCH, HttpChat, HttpEmbedder, HttpReranker};
pub use index::{
    Change, EMB_VERSION, Embedder, Hit, Index, LIVE, Mode, Prepared, Reranker, doc_text, query_text,
};
pub use pg::{GChunk, GSim, PgStore};
// reindex proof
pub fn webhook_proof_marker_zq() {}
