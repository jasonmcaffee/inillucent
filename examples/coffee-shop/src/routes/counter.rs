//! The counter: the menu, orders, payments and the queue.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use super::{blocking, created, AtQuery, Body, Id, Params};
use crate::error::ApiResult;
use crate::store::{LineRequest, NewItem, NewModifier, NewOrder, PayRequest, PriceChange, Store};

/// `?all=true` includes items taken off the menu.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MenuQuery {
    #[serde(default)]
    all: bool,
}

/// `GET /menu`.
pub(super) async fn menu(State(store): State<Store>, Params(query): Params<MenuQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.menu(query.all)).await?))
}

/// `POST /menu/items`.
pub(super) async fn create_item(State(store): State<Store>, Body(item): Body<NewItem>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.create_item(&item)).await?))
}

/// `GET /menu/items/{id}`.
pub(super) async fn item(State(store): State<Store>, Id(id): Id) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.item(id)).await?))
}

/// The body of `PATCH /menu/items/{id}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ActivePatch {
    active: bool,
}

/// `PATCH /menu/items/{id}`.
pub(super) async fn set_item_active(
    State(store): State<Store>, Id(id): Id, Body(patch): Body<ActivePatch>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.set_item_active(id, patch.active)).await?))
}

/// `PUT /menu/items/{id}/prices`.
pub(super) async fn change_prices(State(store): State<Store>, Id(id): Id, Body(prices): Body<PriceChange>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.change_prices(id, &prices)).await?))
}

/// `POST /menu/modifiers`.
pub(super) async fn create_modifier(State(store): State<Store>, Body(modifier): Body<NewModifier>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.create_modifier(&modifier)).await?))
}

/// `?day=&status=` for `GET /orders`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OrdersQuery {
    day: Option<String>,
    status: Option<String>,
}

/// `GET /orders`.
pub(super) async fn orders(State(store): State<Store>, Params(query): Params<OrdersQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(store, move |store| {
            let day = store.day(query.day.as_deref())?;
            store.orders(&day, query.status.as_deref())
        })
        .await?,
    ))
}

/// `POST /orders`.
pub(super) async fn create_order(State(store): State<Store>, Body(order): Body<NewOrder>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.create_order(&order)).await?))
}

/// `?now=` for `GET /orders/queue`: the time to measure waits to.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QueueQuery {
    now: Option<String>,
}

/// `GET /orders/queue`.
pub(super) async fn queue(State(store): State<Store>, Params(query): Params<QueueQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(store, move |store| {
            let now = store.timestamp(query.now.as_deref())?;
            store.queue(&now)
        })
        .await?,
    ))
}

/// `GET /orders/{id}`.
pub(super) async fn order(State(store): State<Store>, Id(id): Id) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.receipt(id)).await?))
}

/// `POST /orders/{id}/lines`.
pub(super) async fn add_line(State(store): State<Store>, Id(id): Id, Body(line): Body<LineRequest>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.add_line(id, &line)).await?))
}

/// The body of `PATCH /orders/{id}/lines/{line}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QuantityPatch {
    quantity: i64,
}

/// `PATCH /orders/{id}/lines/{line}`.
pub(super) async fn change_line(
    State(store): State<Store>, Path((id, line)): Path<(i64, i64)>, Body(patch): Body<QuantityPatch>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.change_line(id, line, patch.quantity)).await?))
}

/// `DELETE /orders/{id}/lines/{line}`.
pub(super) async fn remove_line(State(store): State<Store>, Path((id, line)): Path<(i64, i64)>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.remove_line(id, line)).await?))
}

/// The body of `POST /orders/{id}/promotion`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PromotionBody {
    code: String,
}

/// `POST /orders/{id}/promotion`.
pub(super) async fn apply_promotion(
    State(store): State<Store>, Id(id): Id, Body(body): Body<PromotionBody>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.apply_promotion(id, &body.code)).await?))
}

/// The body of `POST /orders/{id}/redeem`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RedeemBody {
    points: i64,
}

/// `POST /orders/{id}/redeem`.
pub(super) async fn redeem(State(store): State<Store>, Id(id): Id, Body(body): Body<RedeemBody>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.redeem(id, body.points)).await?))
}

/// `POST /orders/{id}/pay`.
pub(super) async fn pay(State(store): State<Store>, Id(id): Id, Body(request): Body<PayRequest>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.pay(id, &request)).await?))
}

/// `POST /orders/{id}/fulfil`.
pub(super) async fn fulfil(State(store): State<Store>, Id(id): Id, Params(query): Params<AtQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.fulfil(id, query.at.as_deref())).await?))
}

/// `POST /orders/{id}/cancel`.
pub(super) async fn cancel(State(store): State<Store>, Id(id): Id, Params(query): Params<AtQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.cancel(id, query.at.as_deref())).await?))
}

/// `POST /orders/{id}/refund`.
pub(super) async fn refund(State(store): State<Store>, Id(id): Id, Params(query): Params<AtQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.refund(id, query.at.as_deref())).await?))
}
