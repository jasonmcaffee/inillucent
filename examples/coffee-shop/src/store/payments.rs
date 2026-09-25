//! Taking money, and what happens to an order after that: pay, fulfil,
//! cancel, refund.
//!
//! Paying is where the database does the most on its own. The service
//! inserts the payments and moves the order to `paid`. The `order_paid`
//! trigger then uses up the stock, posts the sale to the books and moves the
//! customer's points, inside the same transaction. The service checks the
//! books balance before it commits, so a sale is saved with all of its
//! effects or not at all.

use serde::Deserialize;
use serde_json::Value as Json;

use super::orders::{reprice, require_open};
use super::{ensure_balanced, int, json_text, text, Record, Sql, Store};
use crate::error::{ApiError, ApiResult};

/// The body of `POST /orders/{id}/pay`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayRequest {
    /// One payment, or several when the customer splits the bill.
    pub payments: Vec<Tender>,
    /// When it was paid. Now when left out.
    #[serde(default)]
    pub at: Option<String>,
}

/// One payment.
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tender {
    /// `cash` or `card`.
    pub method: String,
    /// What it pays toward the order. Left out on the last payment, it is
    /// whatever the others leave unpaid.
    #[serde(default)]
    pub amount_cents: Option<i64>,
    /// A tip on top, which goes to the staff.
    #[serde(default)]
    pub tip_cents: i64,
    /// For cash, what the customer handed over. The change is worked out.
    #[serde(default)]
    pub tendered_cents: Option<i64>,
}

impl Store {
    /// `POST /orders/{id}/pay`: takes the payments and closes the sale.
    ///
    /// The payments must add up to the order total exactly. They are checked
    /// after they are inserted, with the same `sum()` the books will use, so
    /// the rule and the books cannot disagree.
    ///
    /// @param id - the order
    /// @param request - the payments
    pub fn pay(&self, id: i64, request: &PayRequest) -> ApiResult<Json> {
        let at = self.timestamp(request.at.as_deref())?;
        let tx = self.begin()?;
        require_open(&tx, id)?;
        if tx.integer("SELECT count(*) FROM order_line WHERE order_id = ?1", &[int(id)])? == 0 {
            return Err(ApiError::conflict(format!("order {id} has no lines to pay for")));
        }
        reprice(&tx, id)?;
        let total = tx.integer("SELECT total_cents FROM orders WHERE id = ?1", &[int(id)])?;
        let tenders = fill_last_amount(&request.payments, total)?;
        tx.run(
            "INSERT INTO payment (order_id, method, amount_cents, tip_cents, tendered_cents, at)
             SELECT ?1, value ->> '$.method', value ->> '$.amount_cents', value ->> '$.tip_cents',
                    value ->> '$.tendered_cents', ?2
             FROM json_each(?3)",
            &[int(id), text(&at), json_text(&tenders)?],
        )?;
        let paid = tx.integer("SELECT coalesce(sum(amount_cents), 0) FROM payment WHERE order_id = ?1", &[int(id)])?;
        if paid != total {
            return Err(ApiError::conflict(format!("the payments add up to {paid} cents, and order {id} comes to {total}")));
        }
        tx.run("UPDATE orders SET status = 'paid', paid_at = ?2 WHERE id = ?1", &[int(id), text(&at)])?;
        ensure_balanced(&tx)?;
        tx.commit()?;
        self.receipt(id)
    }

    /// `POST /orders/{id}/fulfil`: the drinks were handed over.
    ///
    /// @param id - the order
    /// @param at - when, or now
    pub fn fulfil(&self, id: i64, at: Option<&str>) -> ApiResult<Json> {
        let at = self.timestamp(at)?;
        self.move_status(id, "UPDATE orders SET status = 'fulfilled', fulfilled_at = ?2 WHERE id = ?1", &at)
    }

    /// `POST /orders/{id}/cancel`: the customer walked away before paying.
    ///
    /// @param id - the order
    /// @param at - when, or now
    pub fn cancel(&self, id: i64, at: Option<&str>) -> ApiResult<Json> {
        let at = self.timestamp(at)?;
        self.move_status(id, "UPDATE orders SET status = 'cancelled', cancelled_at = ?2 WHERE id = ?1", &at)
    }

    /// Runs one status change and answers the receipt.
    ///
    /// The service does not check whether the change is allowed. The
    /// `order_status_moves_forward` trigger refuses a change the lifecycle
    /// does not have, and that refusal becomes `409`.
    ///
    /// @param id - the order
    /// @param sql - the `UPDATE`
    /// @param at - when
    fn move_status(&self, id: i64, sql: &str, at: &str) -> ApiResult<Json> {
        if self.run(sql, &[int(id), text(at)])? == 0 {
            return Err(ApiError::not_found(format!("order {id}")));
        }
        self.receipt(id)
    }

    /// `POST /orders/{id}/refund`: gives the money back.
    ///
    /// Each payment gets a refund payment of the opposite sign, by the same
    /// method, which names it in `refund_of`. `refund_of` is `UNIQUE`, so a
    /// payment cannot be refunded twice. Then the order moves to `refunded`,
    /// and the `order_refunded` trigger reverses the sale in the books and
    /// takes back the points.
    ///
    /// @param id - the order
    /// @param at - when, or now
    pub fn refund(&self, id: i64, at: Option<&str>) -> ApiResult<Json> {
        let at = self.timestamp(at)?;
        let tx = self.begin()?;
        let rows = tx.rows("SELECT status FROM orders WHERE id = ?1", &[int(id)])?;
        let status = Record::first(&rows).map(|row| row.text("status")).ok_or_else(|| ApiError::not_found(format!("order {id}")))?;
        if status != "paid" && status != "fulfilled" {
            return Err(ApiError::conflict(format!("order {id} is {status}, and only a paid order can be refunded")));
        }
        tx.run(
            "INSERT INTO payment (order_id, method, amount_cents, tip_cents, at, refund_of)
             SELECT order_id, method, -amount_cents, -tip_cents, ?2, id
             FROM payment WHERE order_id = ?1 AND refund_of IS NULL
             ORDER BY id",
            &[int(id), text(&at)],
        )?;
        tx.run("UPDATE orders SET status = 'refunded', refunded_at = ?2 WHERE id = ?1", &[int(id), text(&at)])?;
        ensure_balanced(&tx)?;
        tx.commit()?;
        self.receipt(id)
    }
}

/// Fills in the amount of the one payment that left it out, as what the
/// others leave unpaid. An order that points or a promotion paid for in
/// full comes to 0 and takes no payment, because a payment of 0 is refused
/// by the schema.
///
/// @param tenders - the payments as sent
/// @param total - the order total
fn fill_last_amount(tenders: &[Tender], total: i64) -> ApiResult<Vec<Tender>> {
    let open = tenders.iter().filter(|tender| tender.amount_cents.is_none()).count();
    if total == 0 && tenders.is_empty() {
        return Ok(Vec::new());
    }
    if tenders.is_empty() || open > 1 {
        return Err(ApiError::bad_request("send one or more payments, and leave out amount_cents on one of them at most"));
    }
    let named: i64 = tenders.iter().filter_map(|tender| tender.amount_cents).sum();
    Ok(tenders
        .iter()
        .map(|tender| Tender {
            method: tender.method.clone(),
            amount_cents: Some(tender.amount_cents.unwrap_or(total - named)),
            tip_cents: tender.tip_cents,
            tendered_cents: tender.tendered_cents,
        })
        .collect())
}
