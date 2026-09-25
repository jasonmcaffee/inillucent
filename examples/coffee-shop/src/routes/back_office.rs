//! The back office: customers, staff, promotions, stock, purchases, the
//! journal and closing a day.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;

use super::{blocking, created, AtQuery, Body, DayQuery, Id, Params, RangeQuery};
use crate::error::ApiResult;
use crate::store::{CloseDay, Count, ManualEntry, NewCustomer, NewIngredient, NewPromotion, NewPurchase, NewStaff, Store, Waste};

/// `GET /customers`.
pub(super) async fn customers(State(store): State<Store>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, |store| store.customers()).await?))
}

/// `POST /customers`.
pub(super) async fn create_customer(State(store): State<Store>, Body(customer): Body<NewCustomer>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.create_customer(&customer)).await?))
}

/// `GET /customers/{id}`.
pub(super) async fn customer(State(store): State<Store>, Id(id): Id) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.customer(id)).await?))
}

/// `GET /staff`.
pub(super) async fn staff(State(store): State<Store>, Params(query): Params<DayQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(store, move |store| {
            let day = store.day(query.day.as_deref())?;
            store.staff(&day)
        })
        .await?,
    ))
}

/// `POST /staff`.
pub(super) async fn create_staff(State(store): State<Store>, Body(staff): Body<NewStaff>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.create_staff(&staff)).await?))
}

/// `POST /staff/{id}/clock-in`.
pub(super) async fn clock_in(State(store): State<Store>, Id(id): Id, Params(query): Params<AtQuery>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.clock_in(id, query.at.as_deref())).await?))
}

/// `POST /staff/{id}/clock-out`.
pub(super) async fn clock_out(State(store): State<Store>, Id(id): Id, Params(query): Params<AtQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.clock_out(id, query.at.as_deref())).await?))
}

/// `GET /promotions`.
pub(super) async fn promotions(State(store): State<Store>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, |store| store.promotions()).await?))
}

/// `POST /promotions`.
pub(super) async fn create_promotion(State(store): State<Store>, Body(promotion): Body<NewPromotion>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.create_promotion(&promotion)).await?))
}

/// `POST /ingredients`.
pub(super) async fn create_ingredient(State(store): State<Store>, Body(ingredient): Body<NewIngredient>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.create_ingredient(&ingredient)).await?))
}

/// `GET /inventory`.
pub(super) async fn inventory(State(store): State<Store>, Params(query): Params<DayQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(store, move |store| {
            let day = store.day(query.day.as_deref())?;
            store.inventory(&day)
        })
        .await?,
    ))
}

/// `GET /inventory/{id}/movements`.
pub(super) async fn movements(State(store): State<Store>, Id(id): Id) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.movements(id)).await?))
}

/// `POST /inventory/waste`.
pub(super) async fn waste(State(store): State<Store>, Body(waste): Body<Waste>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.waste(&waste)).await?))
}

/// `POST /inventory/count`.
pub(super) async fn count(State(store): State<Store>, Body(count): Body<Count>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.count(&count)).await?))
}

/// `POST /purchases`.
pub(super) async fn receive_purchase(State(store): State<Store>, Body(purchase): Body<NewPurchase>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.receive_purchase(&purchase)).await?))
}

/// `GET /purchases/{id}`.
pub(super) async fn purchase(State(store): State<Store>, Id(id): Id) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.purchase(id)).await?))
}

/// `POST /purchases/{id}/pay`.
pub(super) async fn pay_supplier(State(store): State<Store>, Id(id): Id, Params(query): Params<AtQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.pay_supplier(id, query.at.as_deref())).await?))
}

/// `GET /journal`.
pub(super) async fn journal(State(store): State<Store>, Params(query): Params<DayQuery>) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(store, move |store| {
            let day = store.day(query.day.as_deref())?;
            store.journal(&day)
        })
        .await?,
    ))
}

/// `POST /journal`.
pub(super) async fn manual_entry(State(store): State<Store>, Body(entry): Body<ManualEntry>) -> ApiResult<impl IntoResponse> {
    Ok(created(blocking(store, move |store| store.manual_entry(&entry)).await?))
}

/// `GET /journal/{id}`.
pub(super) async fn entry(State(store): State<Store>, Id(id): Id) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.entry(id)).await?))
}

/// `GET /accounts`.
pub(super) async fn accounts(State(store): State<Store>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, |store| store.accounts()).await?))
}

/// `GET /accounts/{code}/ledger`.
pub(super) async fn account_ledger(
    State(store): State<Store>, Path(code): Path<String>, Params(range): Params<RangeQuery>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, move |store| store.account_ledger(&code, &range.from, &range.to)).await?))
}

/// `POST /days/{day}/close`.
pub(super) async fn close_day(
    State(store): State<Store>, Path(day): Path<String>, Body(close): Body<CloseDay>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        blocking(store, move |store| {
            let day = store.day(Some(&day))?;
            store.close_day(&day, &close)
        })
        .await?,
    ))
}

/// `GET /ledger/check`.
pub(super) async fn reconcile(State(store): State<Store>) -> ApiResult<impl IntoResponse> {
    Ok(Json(blocking(store, |store| store.reconcile()).await?))
}
