//! The REST API: every route, and the handler behind it.
//!
//! A handler does three things: it reads the path, the query string and the
//! body; it calls one method of [`Store`] on a blocking thread; and it writes
//! the answer as JSON. None of them has any SQL in it.
//!
//! | Method and path | What it does |
//! |---|---|
//! | `GET /health` | answers `{"ok": true}` once the database is open |
//! | `GET /people`, `POST /people` | every person with their todo counts; add a person |
//! | `GET /people/{id}`, `DELETE /people/{id}` | one person; delete them, their lists and their todos |
//! | `GET /people/{id}/workload` | their open todos, ranked in the order to do them |
//! | `GET /lists`, `POST /lists` | every list with its counts, busiest first; add a list |
//! | `GET /lists/{id}`, `PATCH /lists/{id}`, `DELETE /lists/{id}` | one list; rename it; delete it and its todos |
//! | `GET /lists/{id}/todos`, `POST /lists/{id}/todos` | the todos in a list, filtered and sorted; add one |
//! | `POST /lists/{id}/toggle-all` | complete every todo in the list, or reopen them all |
//! | `DELETE /lists/{id}/completed` | delete the completed todos |
//! | `PUT /lists/{id}/order` | put the top level todos in a new order |
//! | `GET /todos/{id}`, `PATCH /todos/{id}`, `DELETE /todos/{id}` | a todo with its subtask tree; change it or move it; delete it and its subtasks |
//! | `PUT /todos/{id}/tags` | replace its tags |
//! | `GET /todos/{id}/comments`, `POST /todos/{id}/comments` | its comments; add one |
//! | `GET /todos/{id}/timeline` | everything that happened to it, from the activity log and the comments |
//! | `GET /tags` | every tag with how many todos use it |
//! | `GET /search?q=` | keyword search over titles and notes |
//! | `GET /agenda` | what is overdue, and what is due each day of the next week |
//! | `GET /stats/completions` | todos completed each day, with a running total |
//!
//! ## Why every handler goes through `blocking`
//!
//! A [`Store`] call waits for the database's thread to answer. Waiting on a
//! tokio worker thread would stop that worker from running other requests, so
//! each call runs on tokio's blocking pool with `spawn_blocking`.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::store::{ListPatch, NewComment, NewList, NewPerson, NewTodo, Store, TodoFilter, TodoPatch};

/// Builds the router with every route.
///
/// @param store - the database every handler uses
pub fn router(store: Store) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/people", get(people).post(create_person))
        .route("/people/{id}", get(person).delete(delete_person))
        .route("/people/{id}/workload", get(workload))
        .route("/lists", get(lists).post(create_list))
        .route("/lists/{id}", get(list).patch(rename_list).delete(delete_list))
        .route("/lists/{id}/todos", get(list_todos).post(create_todo))
        .route("/lists/{id}/toggle-all", post(toggle_all))
        .route("/lists/{id}/completed", delete(clear_completed))
        .route("/lists/{id}/order", put(reorder))
        .route("/todos/{id}", get(todo).patch(update_todo).delete(delete_todo))
        .route("/todos/{id}/tags", put(set_tags))
        .route("/todos/{id}/comments", get(comments).post(add_comment))
        .route("/todos/{id}/timeline", get(timeline))
        .route("/tags", get(tags))
        .route("/search", get(search))
        .route("/agenda", get(agenda))
        .route("/stats/completions", get(completions))
        .fallback(|| async { ApiError::not_found("that route") })
        .with_state(store)
}

/// A JSON body. A body that does not parse, or that has a field the type does
/// not know, is answered with `400` and the usual error body.
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
struct Body<T>(T);

/// A query string, refused the same way as a bad body.
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
struct Params<T>(T);

/// The id in the path. `/todos/abc` is a `400`, not a `404`.
struct Id(i64);

impl<S: Send + Sync> FromRequestParts<S> for Id {
    type Rejection = ApiError;

    /// Reads the one path parameter as an integer.
    ///
    /// @param parts - the request's head
    /// @param state - the router's state, unused
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Id, ApiError> {
        let Path(id) = Path::<i64>::from_request_parts(parts, state).await?;
        Ok(Id(id))
    }
}

impl From<JsonRejection> for ApiError {
    /// Turns axum's refusal of a body into the service's error body.
    ///
    /// @param rejection - why axum could not read the body
    fn from(rejection: JsonRejection) -> ApiError {
        ApiError::bad_request(rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    /// Turns axum's refusal of a query string into the service's error body.
    ///
    /// @param rejection - why axum could not read the query string
    fn from(rejection: QueryRejection) -> ApiError {
        ApiError::bad_request(rejection.body_text())
    }
}

impl From<PathRejection> for ApiError {
    /// Turns axum's refusal of a path into the service's error body.
    ///
    /// @param rejection - why axum could not read the id in the path
    fn from(rejection: PathRejection) -> ApiError {
        ApiError::bad_request(rejection.body_text())
    }
}

/// `?today=YYYY-MM-DD`, which every endpoint that talks about "overdue" takes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DayQuery {
    today: Option<String>,
}

/// Runs a store call on tokio's blocking pool and waits for it.
///
/// @param store - the database
/// @param work - the call, which gets the store
async fn blocking<T, F>(store: Store, work: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Store) -> ApiResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || work(&store)).await.map_err(|error| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "panic",
        message: format!("the request stopped: {error}"),
    })?
}

/// Answers `201 Created` with a JSON body.
///
/// @param value - the new row
fn created<T: serde::Serialize>(value: T) -> impl IntoResponse {
    (StatusCode::CREATED, Json(value))
}

/// `GET /health`.
async fn health(State(store): State<Store>) -> ApiResult<Json<Value>> {
    let today = blocking(store, |store| store.today(None)).await?;
    Ok(Json(json!({ "ok": true, "today": today })))
}

/// `GET /people`.
async fn people(State(store): State<Store>, Params(query): Params<DayQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.people(query.today.as_deref())).await?))
}

/// `POST /people`.
async fn create_person(State(store): State<Store>, Body(person): Body<NewPerson>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.create_person(&person)).await?))
}

/// `GET /people/{id}`.
async fn person(State(store): State<Store>, Id(id): Id, Params(query): Params<DayQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.person(id, query.today.as_deref())).await?))
}

/// `DELETE /people/{id}`.
async fn delete_person(State(store): State<Store>, Id(id): Id) -> ApiResult<impl IntoResponse> {
    blocking(store, move |store| store.delete_person(id)).await?;
    Ok(Json(json!({ "deleted": id })))
}

/// `GET /people/{id}/workload`.
async fn workload(State(store): State<Store>, Id(id): Id, Params(query): Params<DayQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.workload(id, query.today.as_deref())).await?))
}

/// `GET /lists`.
async fn lists(State(store): State<Store>, Params(query): Params<DayQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.lists(query.today.as_deref())).await?))
}

/// `POST /lists`.
async fn create_list(State(store): State<Store>, Body(list): Body<NewList>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.create_list(&list)).await?))
}

/// `GET /lists/{id}`.
async fn list(State(store): State<Store>, Id(id): Id, Params(query): Params<DayQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.list(id, query.today.as_deref())).await?))
}

/// `PATCH /lists/{id}`.
async fn rename_list(State(store): State<Store>, Id(id): Id, Body(patch): Body<ListPatch>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.rename_list(id, &patch)).await?))
}

/// `DELETE /lists/{id}`.
async fn delete_list(State(store): State<Store>, Id(id): Id) -> ApiResult<impl IntoResponse> {
    let todos = blocking(store, move |store| store.delete_list(id)).await?;
    Ok(Json(json!({ "deleted": id, "todos_deleted": todos })))
}

/// `GET /lists/{id}/todos`.
async fn list_todos(State(store): State<Store>, Id(id): Id, Params(filter): Params<TodoFilter>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.list_todos(id, &filter)).await?))
}

/// `POST /lists/{id}/todos`.
async fn create_todo(State(store): State<Store>, Id(id): Id, Body(todo): Body<NewTodo>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.create_todo(id, &todo)).await?))
}

/// The body of `POST /lists/{id}/toggle-all`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToggleAll {
    completed: bool,
}

/// `POST /lists/{id}/toggle-all`.
async fn toggle_all(State(store): State<Store>, Id(id): Id, Body(toggle): Body<ToggleAll>) -> ApiResult<impl IntoResponse> {
    let changed = blocking(store, move |store| store.toggle_all(id, toggle.completed)).await?;
    Ok(Json(json!({ "changed": changed })))
}

/// `DELETE /lists/{id}/completed`.
async fn clear_completed(State(store): State<Store>, Id(id): Id) -> ApiResult<impl IntoResponse> {
    let deleted = blocking(store, move |store| store.clear_completed(id)).await?;
    Ok(Json(json!({ "todos_deleted": deleted })))
}

/// The body of `PUT /lists/{id}/order`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Order {
    todo_ids: Vec<i64>,
}

/// `PUT /lists/{id}/order`.
async fn reorder(State(store): State<Store>, Id(id): Id, Body(order): Body<Order>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.reorder(id, &order.todo_ids)).await?))
}

/// `GET /todos/{id}`.
async fn todo(State(store): State<Store>, Id(id): Id, Params(query): Params<DayQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.todo_detail(id, query.today.as_deref())).await?))
}

/// `PATCH /todos/{id}`.
async fn update_todo(State(store): State<Store>, Id(id): Id, Body(patch): Body<TodoPatch>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.update_todo(id, &patch)).await?))
}

/// `DELETE /todos/{id}`.
async fn delete_todo(State(store): State<Store>, Id(id): Id) -> ApiResult<impl IntoResponse> {
    let deleted = blocking(store, move |store| store.delete_todo(id)).await?;
    Ok(Json(json!({ "todos_deleted": deleted })))
}

/// The body of `PUT /todos/{id}/tags`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tags {
    tags: Vec<String>,
}

/// `PUT /todos/{id}/tags`.
async fn set_tags(State(store): State<Store>, Id(id): Id, Body(tags): Body<Tags>) -> ApiResult<impl IntoResponse> {
    let tags = blocking(store, move |store| store.set_tags(id, &tags.tags)).await?;
    Ok(Json(json!({ "tags": tags })))
}

/// `GET /todos/{id}/comments`.
async fn comments(State(store): State<Store>, Id(id): Id) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.comments(id)).await?))
}

/// `POST /todos/{id}/comments`.
async fn add_comment(State(store): State<Store>, Id(id): Id, Body(comment): Body<NewComment>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.add_comment(id, &comment)).await?))
}

/// `GET /todos/{id}/timeline`.
async fn timeline(State(store): State<Store>, Id(id): Id) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.timeline(id)).await?))
}

/// `GET /tags`.
async fn tags(State(store): State<Store>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, |store| store.tags()).await?))
}

/// The query string of `GET /search`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchQuery {
    q: String,
    list_id: Option<i64>,
    limit: Option<i64>,
}

/// `GET /search`.
async fn search(State(store): State<Store>, Params(query): Params<SearchQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.search(&query.q, query.list_id, query.limit.unwrap_or(20))).await?))
}

/// The query string of `GET /agenda`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgendaQuery {
    from: Option<String>,
    days: Option<i64>,
    assignee_id: Option<i64>,
}

/// `GET /agenda`.
async fn agenda(State(store): State<Store>, Params(query): Params<AgendaQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.agenda(query.from.as_deref(), query.days.unwrap_or(7), query.assignee_id)).await?))
}

/// The query string of `GET /stats/completions`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionsQuery {
    until: Option<String>,
    days: Option<i64>,
}

/// `GET /stats/completions`.
async fn completions(State(store): State<Store>, Params(query): Params<CompletionsQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.completions(query.until.as_deref(), query.days.unwrap_or(14))).await?))
}
