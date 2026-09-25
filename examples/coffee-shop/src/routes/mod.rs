//! The REST API: every route, and the handler behind it.
//!
//! A handler does three things: it reads the path, the query string and the
//! body; it calls one method of [`Store`] on a blocking thread; and it writes
//! the answer as JSON. None of them has any SQL in it.
//!
//! | File | Routes |
//! |---|---|
//! | `counter.rs` | the menu, orders, payments, the queue |
//! | `back_office.rs` | customers, staff, promotions, stock, purchases, the journal, closing a day |
//! | `reports.rs` | the Z report and every `/reports` route |
//!
//! ## Why every handler goes through `blocking`
//!
//! A [`Store`] call waits for the database's thread to answer. Waiting on a
//! tokio worker thread would stop that worker from running other requests, so
//! each call runs on tokio's blocking pool with `spawn_blocking`.

mod back_office;
mod counter;
mod reports;

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::{ApiError, ApiResult};
use crate::store::Store;

/// Builds the router with every route.
///
/// @param store - the database every handler uses
pub fn router(store: Store) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/menu", get(counter::menu))
        .route("/menu/items", post(counter::create_item))
        .route("/menu/items/{id}", get(counter::item).patch(counter::set_item_active))
        .route("/menu/items/{id}/prices", put(counter::change_prices))
        .route("/menu/modifiers", post(counter::create_modifier))
        .route("/orders", get(counter::orders).post(counter::create_order))
        .route("/orders/queue", get(counter::queue))
        .route("/orders/{id}", get(counter::order))
        .route("/orders/{id}/lines", post(counter::add_line))
        .route("/orders/{id}/lines/{line}", patch(counter::change_line).delete(counter::remove_line))
        .route("/orders/{id}/promotion", post(counter::apply_promotion))
        .route("/orders/{id}/redeem", post(counter::redeem))
        .route("/orders/{id}/pay", post(counter::pay))
        .route("/orders/{id}/fulfil", post(counter::fulfil))
        .route("/orders/{id}/cancel", post(counter::cancel))
        .route("/orders/{id}/refund", post(counter::refund))
        .route("/customers", get(back_office::customers).post(back_office::create_customer))
        .route("/customers/{id}", get(back_office::customer))
        .route("/staff", get(back_office::staff).post(back_office::create_staff))
        .route("/staff/{id}/clock-in", post(back_office::clock_in))
        .route("/staff/{id}/clock-out", post(back_office::clock_out))
        .route("/promotions", get(back_office::promotions).post(back_office::create_promotion))
        .route("/ingredients", post(back_office::create_ingredient))
        .route("/inventory", get(back_office::inventory))
        .route("/inventory/{id}/movements", get(back_office::movements))
        .route("/inventory/waste", post(back_office::waste))
        .route("/inventory/count", post(back_office::count))
        .route("/purchases", post(back_office::receive_purchase))
        .route("/purchases/{id}", get(back_office::purchase))
        .route("/purchases/{id}/pay", post(back_office::pay_supplier))
        .route("/journal", get(back_office::journal).post(back_office::manual_entry))
        .route("/journal/{id}", get(back_office::entry))
        .route("/accounts", get(back_office::accounts))
        .route("/accounts/{code}/ledger", get(back_office::account_ledger))
        .route("/days/{day}", get(reports::z_report))
        .route("/days/{day}/close", post(back_office::close_day))
        .route("/ledger/check", get(back_office::reconcile))
        .route("/reports/sales", get(reports::sales))
        .route("/reports/hourly", get(reports::hourly))
        .route("/reports/items", get(reports::items))
        .route("/reports/margins", get(reports::margins))
        .route("/reports/tips", get(reports::tips))
        .route("/reports/trial-balance", get(reports::trial_balance))
        .route("/reports/income-statement", get(reports::income_statement))
        .route("/reports/balance-sheet", get(reports::balance_sheet))
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

/// The id in the path. `/orders/abc` is a `400`, not a `404`.
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
    /// @param rejection - why axum could not read the path
    fn from(rejection: PathRejection) -> ApiError {
        ApiError::bad_request(rejection.body_text())
    }
}

/// `?at=`: when an action happened, for the routes that take no body. Now
/// when left out.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AtQuery {
    at: Option<String>,
}

/// `?day=YYYY-MM-DD`. Today when left out.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DayQuery {
    day: Option<String>,
}

/// `?from=&to=`: a range of days, both included.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeQuery {
    from: String,
    to: String,
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
async fn health(State(store): State<Store>) -> ApiResult<Json<serde_json::Value>> {
    let now = blocking(store, |store| store.timestamp(None)).await?;
    Ok(Json(json!({ "ok": true, "now": now })))
}
