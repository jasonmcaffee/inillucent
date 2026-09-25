//! The Z report and every `/reports` route.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use super::{blocking, DayQuery, Params, RangeQuery};
use crate::error::ApiResult;
use crate::store::Store;

/// `GET /days/{day}`.
pub(super) async fn z_report(State(store): State<Store>, Path(day): Path<String>) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(store, move |store| {
            let day = store.day(Some(&day))?;
            store.z_report(&day)
        })
        .await?,
    ))
}

/// `GET /reports/sales`.
pub(super) async fn sales(State(store): State<Store>, Params(range): Params<RangeQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.sales_by_day(&range.from, &range.to)).await?))
}

/// `GET /reports/hourly`.
pub(super) async fn hourly(State(store): State<Store>, Params(query): Params<DayQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(store, move |store| {
            let day = store.day(query.day.as_deref())?;
            store.sales_by_hour(&day)
        })
        .await?,
    ))
}

/// `?from=&to=&top=` for `GET /reports/items`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ItemsQuery {
    from: String,
    to: String,
    top: Option<i64>,
}

/// `GET /reports/items`.
pub(super) async fn items(State(store): State<Store>, Params(query): Params<ItemsQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.items(&query.from, &query.to, query.top)).await?))
}

/// `GET /reports/margins`.
pub(super) async fn margins(State(store): State<Store>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, |store| store.margins()).await?))
}

/// `GET /reports/tips`.
pub(super) async fn tips(State(store): State<Store>, Params(query): Params<DayQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(store, move |store| {
            let day = store.day(query.day.as_deref())?;
            store.tip_pool(&day)
        })
        .await?,
    ))
}

/// `?as_of=` for the statements that describe the end of a day.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AsOfQuery {
    as_of: Option<String>,
}

/// `GET /reports/trial-balance`.
pub(super) async fn trial_balance(State(store): State<Store>, Params(query): Params<AsOfQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(store, move |store| {
            let day = store.day(query.as_of.as_deref())?;
            store.trial_balance(&day)
        })
        .await?,
    ))
}

/// `GET /reports/income-statement`.
pub(super) async fn income_statement(State(store): State<Store>, Params(range): Params<RangeQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.income_statement(&range.from, &range.to)).await?))
}

/// `GET /reports/balance-sheet`.
pub(super) async fn balance_sheet(State(store): State<Store>, Params(query): Params<AsOfQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(store, move |store| {
            let day = store.day(query.as_of.as_deref())?;
            store.balance_sheet(&day)
        })
        .await?,
    ))
}
