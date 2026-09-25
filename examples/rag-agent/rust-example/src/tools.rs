//! The five tools the agent sees: their descriptions, their parameters, and
//! what each one does.
//!
//! The descriptions are written for the agent. A client puts them in the
//! model's context, and they are how the model decides which tool to call and
//! with what. Each one says when to use the tool and what the result means.
//!
//! Every result is sent twice: as `structuredContent`, a JSON object a client
//! can read field by field, and as the same JSON written out as text in
//! `content`, for a client that reads only text. A tool that fails returns a
//! result with `isError: true` and a message the agent can act on. A call to a
//! tool that does not exist is a JSON-RPC error instead, as the specification
//! says.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::mcp::RpcError;
use crate::scheduler::Scheduler;
use crate::search::{search, Mode, SearchRequest};
use crate::store::Store;

/// What the tools work on.
pub struct Tools {
    /// The database.
    pub store: Store,
    /// The sync thread.
    pub scheduler: Scheduler,
    /// The mode `search` uses when the agent names none.
    pub default_mode: Mode,
}

/// The arguments of `search`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    query: String,
    mode: Option<Mode>,
    k: Option<usize>,
    title: Option<String>,
}

/// The arguments of `get_passage`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PassageArguments {
    chunk_id: i64,
    neighbors: Option<i64>,
}

/// The arguments of `list_documents`.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ListArguments {
    filter: Option<String>,
}

impl Tools {
    /// Returns the `tools/list` entries: each tool's name, description and input schema.
    pub fn definitions() -> Value {
        json!([
            {
                "name": "search",
                "title": "Search the philosophy articles",
                "description": "Finds the passages that best answer a question, from 80 Wikipedia articles on Greek and \
                    Roman philosophy. Call it before answering any question on the subject. Each hit has the article's \
                    title and url to cite, a chunk_id for get_passage, and a cosine `distance` from the question. A best \
                    distance above about 0.4 means the articles are about something else. A smaller distance does not \
                    prove the articles answer the question: read the passages.\n\nModes: `rrf` (the default) fuses a \
                    search by meaning and a search by keyword with reciprocal rank fusion. `vector` ranks by meaning \
                    only. `keyword` ranks by the question's words only; use it for an exact name or term such as \
                    Metrodorus or ataraxia. `hybrid` lets the database combine meaning and keywords in one query and \
                    adds `score`, `confidence` and `origin` to each hit.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "The question, in plain words." },
                        "mode": { "type": "string", "enum": ["hybrid", "vector", "keyword", "rrf"], "description": "How to rank. Defaults to rrf." },
                        "k": { "type": "integer", "minimum": 1, "maximum": 20, "description": "How many passages to return. Defaults to 5." },
                        "title": { "type": "string", "description": "Only search the article with exactly this title, as list_documents shows it." }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                },
                "annotations": { "readOnlyHint": true }
            },
            {
                "name": "get_passage",
                "title": "Read a passage with its context",
                "description": "Returns the passage a search hit came from together with the passages on either side, \
                    as one continuous piece of the article. Use it when a hit stops before the answer does.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "chunk_id": { "type": "integer", "description": "The chunk_id of a search hit." },
                        "neighbors": { "type": "integer", "minimum": 0, "maximum": 3, "description": "How many passages to add on each side. Defaults to 1." }
                    },
                    "required": ["chunk_id"],
                    "additionalProperties": false
                },
                "annotations": { "readOnlyHint": true }
            },
            {
                "name": "list_documents",
                "title": "List the articles",
                "description": "Lists the articles in the index with their urls and chunk counts. Use it to check \
                    whether a person or subject has an article at all, or to find the exact title for search's `title`.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "filter": { "type": "string", "description": "Only titles containing this text, ignoring case." } },
                    "additionalProperties": false
                },
                "annotations": { "readOnlyHint": true }
            },
            {
                "name": "sync_status",
                "title": "Show the sync status",
                "description": "Reports whether the index is being updated from the source documents now, what the \
                    last update changed, and when the next one is due.",
                "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
                "annotations": { "readOnlyHint": true }
            },
            {
                "name": "sync_now",
                "title": "Update the index now",
                "description": "Starts an update of the index from the source documents without waiting for the timer. \
                    It returns at once; call sync_status to follow it. Only changed documents are embedded again.",
                "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
                "annotations": { "readOnlyHint": false, "idempotentHint": true }
            }
        ])
    }

    /// Runs one `tools/call` request.
    ///
    /// @param params - the request's `params`: the tool's `name` and its `arguments`
    pub fn call(&self, params: &Value) -> Result<Value, RpcError> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let outcome = match name {
            "search" => self.search(arguments),
            "get_passage" => self.get_passage(arguments),
            "list_documents" => self.list_documents(arguments),
            "sync_status" => self.sync_status(),
            "sync_now" => Ok(self.sync_now()),
            other => return Err(RpcError::new(-32602, format!("no tool named `{other}`"))),
        };
        Ok(match outcome {
            Ok(value) => json!({
                "content": [{ "type": "text", "text": value.to_string() }],
                "structuredContent": value,
                "isError": false
            }),
            Err(message) => json!({ "content": [{ "type": "text", "text": message }], "isError": true }),
        })
    }

    /// `search`: runs the search and adds the state of the index.
    ///
    /// The index state tells the agent when a sync is still filling the index,
    /// so a thin result during the first sync is not mistaken for a thin corpus.
    ///
    /// @param arguments - the tool's arguments
    fn search(&self, arguments: Value) -> Result<Value, String> {
        let arguments: SearchArguments = parse(arguments)?;
        let request = SearchRequest {
            query: arguments.query,
            mode: arguments.mode.unwrap_or(self.default_mode),
            k: arguments.k.unwrap_or(5),
            title: arguments.title,
        };
        let result = search(&self.store, &request)?;
        let mut value = serde_json::to_value(result).map_err(|error| error.to_string())?;
        value["index"] = self.index_state()?;
        Ok(value)
    }

    /// `get_passage`: a chunk and its neighbours as one span of the article.
    ///
    /// @param arguments - the tool's arguments
    fn get_passage(&self, arguments: Value) -> Result<Value, String> {
        let arguments: PassageArguments = parse(arguments)?;
        let neighbors = arguments.neighbors.unwrap_or(1).clamp(0, 3);
        match self.store.passage(arguments.chunk_id, neighbors)? {
            Some(passage) => serde_json::to_value(passage).map_err(|error| error.to_string()),
            None => Err(format!("there is no chunk {}. Take a chunk_id from a search hit", arguments.chunk_id)),
        }
    }

    /// `list_documents`: the articles, with chunk counts.
    ///
    /// @param arguments - the tool's arguments
    fn list_documents(&self, arguments: Value) -> Result<Value, String> {
        let arguments: ListArguments = if arguments.is_null() { ListArguments::default() } else { parse(arguments)? };
        let documents = self.store.documents(arguments.filter.as_deref().unwrap_or(""))?;
        Ok(json!({ "count": documents.len(), "documents": documents, "index": self.index_state()? }))
    }

    /// `sync_status`: the sync thread's state, the table sizes, and the last recorded sync.
    ///
    /// The last recorded sync is read from `sync_log`, so it survives a
    /// restart. The table sizes show that `chunk` and `chunk_search` hold the
    /// same chunks and that every chunk has a 768 number vector.
    fn sync_status(&self) -> Result<Value, String> {
        let mut status = self.scheduler.status();
        status["tables"] = self.store.table_counts()?;
        let recorded = self.store.last_sync()?.map(|text| serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)));
        status["last_recorded_sync"] = recorded.unwrap_or(Value::Null);
        Ok(status)
    }

    /// `sync_now`: starts a sync unless one is running.
    fn sync_now(&self) -> Value {
        let started = self.scheduler.request_sync();
        let message = if started { "a sync has started" } else { "a sync is already running" };
        json!({ "started": started, "message": message, "progress": self.scheduler.progress() })
    }

    /// Returns how many documents and chunks are indexed, and the running sync's progress.
    fn index_state(&self) -> Result<Value, String> {
        let (documents, chunks) = self.store.counts()?;
        Ok(json!({ "documents": documents, "chunks": chunks, "sync": self.scheduler.progress() }))
    }
}

/// Reads a tool's arguments into a typed struct.
///
/// serde's messages name the field and what was wrong with it, such as
/// ``unknown variant `semantic`, expected one of `hybrid`, `vector`, `keyword`, `rrf` ``,
/// which is what the agent needs to correct its call.
///
/// @param arguments - the arguments as JSON
fn parse<T: serde::de::DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|error| format!("bad arguments: {error}"))
}
