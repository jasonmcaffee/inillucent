//! Orders at the counter: open one, add and change lines, apply a promotion
//! code or loyalty points, price it, and read it back as a receipt.
//!
//! An order is `open` while the customer is still choosing. Its lines copy
//! the menu price when they are added, so a price change never reaches an
//! order in progress. Every change to an open order runs [`reprice`], which
//! writes the subtotal, the discount and the tax in three `UPDATE`s.
//! `payments.rs` moves the order on from `open`.

use serde::Deserialize;
use serde_json::Value as Json;

use super::menu::price_of;
use super::{int, json_text, opt_int, opt_text, text, Record, Sql, Store};
use crate::error::{ApiError, ApiResult};

/// The body of `POST /orders`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewOrder {
    /// The loyalty customer, if the customer gave one.
    #[serde(default)]
    pub customer_id: Option<i64>,
    /// Who took the order.
    #[serde(default)]
    pub staff_id: Option<i64>,
    /// `counter` or `mobile`.
    #[serde(default)]
    pub channel: Option<String>,
    /// When it was opened. Now when left out.
    #[serde(default)]
    pub at: Option<String>,
    /// What was ordered.
    #[serde(default)]
    pub lines: Vec<LineRequest>,
    /// A promotion code.
    #[serde(default)]
    pub promotion_code: Option<String>,
    /// Loyalty points to spend, in hundreds.
    #[serde(default)]
    pub redeem_points: Option<i64>,
}

/// One line: an item, a size, how many, and the modifiers on each.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct LineRequest {
    /// The menu item.
    pub menu_item_id: i64,
    /// `small`, `medium`, `large`, or `regular` for an item sold in one size.
    #[serde(default = "regular")]
    pub size: String,
    /// How many. 1 when left out.
    #[serde(default = "one")]
    pub quantity: i64,
    /// Modifier ids, such as oat milk and an extra shot.
    #[serde(default)]
    pub modifiers: Vec<i64>,
}

/// serde's default for `size`.
fn regular() -> String {
    "regular".to_string()
}

/// serde's default for `quantity`.
fn one() -> i64 {
    1
}

/// Adds a line, or adds to the quantity of the same line already there.
///
/// The price of one is the size's price plus every modifier's price, worked out
/// in the `SELECT`. `modifier_key` is the modifier ids sorted and joined, so
/// the same drink with the same modifiers in any order has the same key, and
/// the `UNIQUE` constraint on `(order_id, menu_item_id, size, modifier_key)`
/// sends a second one into `DO UPDATE`, which adds the quantities. `excluded`
/// is the row that would have been inserted.
const ADD_LINE: &str = "
INSERT INTO order_line (order_id, menu_item_id, size, modifier_key, quantity, unit_price_cents, taxable)
SELECT ?1, p.menu_item_id, p.size,
       coalesce((SELECT group_concat(value, ',' ORDER BY value) FROM (SELECT DISTINCT value FROM json_each(?4))), ''),
       ?5,
       p.price_cents + coalesce((SELECT sum(price_cents) FROM modifier WHERE id IN (SELECT value FROM json_each(?4))), 0),
       m.taxable
FROM menu_price p
JOIN menu_item m ON m.id = p.menu_item_id
WHERE p.menu_item_id = ?2 AND p.size = ?3
ON CONFLICT (order_id, menu_item_id, size, modifier_key) DO UPDATE SET quantity = quantity + excluded.quantity
RETURNING id, quantity";

/// The discount: the promotion, when the order qualifies for it on its day,
/// plus 500 cents for every 100 points spent, and never more than the
/// subtotal. `min()` with two arguments is the scalar minimum, not the
/// aggregate.
const PRICE_DISCOUNT: &str = "
UPDATE orders SET discount_cents = min(subtotal_cents,
  coalesce((SELECT CASE p.kind WHEN 'percent' THEN (orders.subtotal_cents * p.value + 50) / 100 ELSE p.value END
            FROM promotion p
            WHERE p.id = orders.promotion_id
              AND orders.business_day BETWEEN p.starts_on AND p.ends_on
              AND orders.subtotal_cents >= p.min_subtotal_cents), 0)
  + redeem_points / 100 * 500)
WHERE id = ?1";

/// The tax: the rate applied to the taxed lines, after their share of the
/// discount, rounded half up to the cent. `+ 5000) / 10000` rounds a basis
/// point product with integer arithmetic, so no `REAL` is involved.
const PRICE_TAX: &str = "
UPDATE orders SET tax_cents = coalesce(
  (SELECT ((t.taxed - orders.discount_cents * t.taxed / orders.subtotal_cents) * s.value + 5000) / 10000
   FROM (SELECT sum(line_total_cents) AS taxed FROM order_line WHERE order_id = ?1 AND taxable = 1) t,
        setting s
   WHERE s.key = 'tax_rate_bp' AND orders.subtotal_cents > 0), 0)
WHERE id = ?1";

/// An order's head: the order, who bought it, who served it, and its code.
///
/// This is the `order_summary` view written out. Reading the view with
/// `WHERE id = ?1` is the obvious form, and in inillucent 1.0.30 it builds
/// every order in the view before it picks one (task-2134 item 11): 2.5 ms
/// against 0.05 ms on the seed data, growing with every order taken.
const RECEIPT: &str = "
SELECT o.id, o.business_day, o.ticket, o.channel, o.status,
       o.customer_id, c.name AS customer_name, o.staff_id, s.name AS staff_name,
       o.subtotal_cents, o.discount_cents, o.tax_cents, o.total_cents, o.redeem_points, p.code AS promotion_code,
       o.opened_at, o.paid_at, o.fulfilled_at, o.cancelled_at, o.refunded_at
FROM orders o
LEFT JOIN customer c ON c.id = o.customer_id
LEFT JOIN staff s ON s.id = o.staff_id
LEFT JOIN promotion p ON p.id = o.promotion_id
WHERE o.id = ?1";

/// The lines of an order, with each line's modifiers named in one string.
const RECEIPT_LINES: &str = "
SELECT l.id, l.menu_item_id, m.sku, m.name, l.size, l.quantity, l.unit_price_cents, l.line_total_cents, l.taxable,
       group_concat(md.name, ', ' ORDER BY md.name) AS modifiers
FROM order_line l
JOIN menu_item m ON m.id = l.menu_item_id
LEFT JOIN order_line_modifier lm ON lm.line_id = l.id
LEFT JOIN modifier md ON md.id = lm.modifier_id
WHERE l.order_id = ?1
GROUP BY l.id
ORDER BY l.id";

/// The paid orders still to be made, oldest first, with how long each has
/// waited and what is in it. The lines are joined and folded into one string
/// per order by `group_concat(... ORDER BY l.id)`, and `row_number()` over
/// the grouped rows is the place in the queue.
///
/// The obvious form reads the items with a correlated subquery in the
/// select list. inillucent 1.0.30 refuses a correlated subquery used as a
/// value in a `SELECT` that also has a window function (task-2134 item 8),
/// so this joins and groups instead. It reads `orders` and not the
/// `order_summary` view, because inillucent 1.0.30 builds every row of a view
/// before it applies the `WHERE` (task-2134 item 11), and the index on
/// `orders (status, paid_at)` would go unused.
const QUEUE: &str = "
SELECT o.id, o.ticket, o.channel, c.name AS customer_name, o.paid_at,
       (unixepoch(?1) - unixepoch(o.paid_at)) / 60 AS waiting_minutes,
       group_concat(l.quantity || ' ' || l.size || ' ' || m.name, ', ' ORDER BY l.id) AS items,
       row_number() OVER (ORDER BY o.paid_at, o.id) AS place
FROM orders o
LEFT JOIN customer c ON c.id = o.customer_id
JOIN order_line l ON l.order_id = o.id
JOIN menu_item m ON m.id = l.menu_item_id
WHERE o.status = 'paid'
GROUP BY o.id
ORDER BY place";

impl Store {
    /// `POST /orders`: opens an order and adds its lines, in one transaction.
    ///
    /// The ticket number is one more than the largest on the same business
    /// day, read by the `INSERT ... SELECT` itself. `business_day` is a
    /// generated column, so the `WHERE` names the same expression it is made
    /// from. `UNIQUE (business_day, ticket)` stops two orders sharing a number.
    ///
    /// @param order - the new order
    pub fn create_order(&self, order: &NewOrder) -> ApiResult<Json> {
        let at = self.timestamp(order.at.as_deref())?;
        let tx = self.begin()?;
        let id = tx.integer(
            "INSERT INTO orders (opened_at, ticket, channel, customer_id, staff_id)
             SELECT ?1, coalesce(max(ticket), 0) + 1, coalesce(?2, 'counter'), ?3, ?4
             FROM orders WHERE business_day = date(?1)
             RETURNING id",
            &[text(&at), opt_text(order.channel.as_deref()), opt_int(order.customer_id), opt_int(order.staff_id)],
        )?;
        for line in &order.lines {
            add_line(&tx, id, line)?;
        }
        if let Some(code) = &order.promotion_code {
            set_promotion(&tx, id, code)?;
        }
        if let Some(points) = order.redeem_points {
            set_redeem(&tx, id, points)?;
        }
        reprice(&tx, id)?;
        check_promotion_qualifies(&tx, id)?;
        tx.commit()?;
        self.receipt(id)
    }

    /// `POST /orders/{id}/lines`: adds a line, or more of one already there.
    ///
    /// @param id - the order
    /// @param line - the line
    pub fn add_line(&self, id: i64, line: &LineRequest) -> ApiResult<Json> {
        let tx = self.begin()?;
        require_order(&tx, id)?;
        add_line(&tx, id, line)?;
        reprice(&tx, id)?;
        tx.commit()?;
        self.receipt(id)
    }

    /// `PATCH /orders/{id}/lines/{line}`: sets a line's quantity.
    ///
    /// The `order_line_frozen_after_open` trigger refuses it once the order is paid.
    ///
    /// @param id - the order
    /// @param line - the line
    /// @param quantity - the new quantity
    pub fn change_line(&self, id: i64, line: i64, quantity: i64) -> ApiResult<Json> {
        let tx = self.begin()?;
        let changed =
            tx.run("UPDATE order_line SET quantity = ?3 WHERE id = ?2 AND order_id = ?1", &[int(id), int(line), int(quantity)])?;
        if changed == 0 {
            return Err(ApiError::not_found(format!("line {line} of order {id}")));
        }
        reprice(&tx, id)?;
        tx.commit()?;
        self.receipt(id)
    }

    /// `DELETE /orders/{id}/lines/{line}`: removes a line and its modifiers.
    ///
    /// @param id - the order
    /// @param line - the line
    pub fn remove_line(&self, id: i64, line: i64) -> ApiResult<Json> {
        let tx = self.begin()?;
        let removed = tx.run("DELETE FROM order_line WHERE id = ?2 AND order_id = ?1", &[int(id), int(line)])?;
        if removed == 0 {
            return Err(ApiError::not_found(format!("line {line} of order {id}")));
        }
        reprice(&tx, id)?;
        tx.commit()?;
        self.receipt(id)
    }

    /// `POST /orders/{id}/promotion`: applies a promotion code.
    ///
    /// @param id - the order
    /// @param code - the code, in any case
    pub fn apply_promotion(&self, id: i64, code: &str) -> ApiResult<Json> {
        let tx = self.begin()?;
        require_open(&tx, id)?;
        set_promotion(&tx, id, code)?;
        reprice(&tx, id)?;
        check_promotion_qualifies(&tx, id)?;
        tx.commit()?;
        self.receipt(id)
    }

    /// `POST /orders/{id}/redeem`: spends loyalty points on an order.
    ///
    /// @param id - the order
    /// @param points - how many, a multiple of 100; 0 takes them back off
    pub fn redeem(&self, id: i64, points: i64) -> ApiResult<Json> {
        let tx = self.begin()?;
        require_open(&tx, id)?;
        set_redeem(&tx, id, points)?;
        reprice(&tx, id)?;
        tx.commit()?;
        self.receipt(id)
    }

    /// `GET /orders/{id}`: the order as a receipt.
    ///
    /// @param id - the order
    pub fn receipt(&self, id: i64) -> ApiResult<Json> {
        let mut receipt = self.object(RECEIPT, &[int(id)], &format!("order {id}"))?;
        receipt["lines"] = Json::from(self.objects(RECEIPT_LINES, &[int(id)], &[])?);
        receipt["payments"] = Json::from(self.objects(
            "SELECT id, method, amount_cents, tip_cents, tendered_cents, change_cents, at, refund_of
             FROM payment WHERE order_id = ?1 ORDER BY id",
            &[int(id)],
            &[],
        )?);
        receipt["points"] = self.object(
            "SELECT coalesce(sum(points) FILTER (WHERE reason = 'earn'), 0) AS earned,
                    coalesce(-sum(points) FILTER (WHERE reason = 'redeem'), 0) AS redeemed,
                    coalesce(sum(points) FILTER (WHERE reason = 'refund'), 0) AS reversed,
                    (SELECT points FROM customer_points WHERE customer_id = ?2) AS balance
             FROM loyalty_entry WHERE order_id = ?1",
            &[int(id), receipt["customer_id"].as_i64().map(int).unwrap_or(super::Value::Null)],
            "points",
        )?;
        Ok(receipt)
    }

    /// `GET /orders`: the orders of one business day, optionally of one status.
    ///
    /// @param day - the business day
    /// @param status - one status, or every status
    pub fn orders(&self, day: &str, status: Option<&str>) -> ApiResult<Vec<Json>> {
        self.objects(
            "SELECT * FROM order_summary WHERE business_day = ?1 AND (?2 IS NULL OR status = ?2) ORDER BY ticket",
            &[text(day), opt_text(status)],
            &[],
        )
    }

    /// `GET /orders/queue`: the paid orders the baristas still have to make.
    ///
    /// @param now - the time to measure the wait to
    pub fn queue(&self, now: &str) -> ApiResult<Vec<Json>> {
        self.objects(QUEUE, &[text(now)], &[])
    }
}

/// Adds one line to an open order, with its modifiers.
///
/// The item's price is read first so that a missing size or an item off the
/// menu gets a clear `404` or `409`, instead of an `INSERT ... SELECT` that
/// quietly inserts nothing.
///
/// @param sql - the open transaction
/// @param order - the order
/// @param line - the line
fn add_line(sql: &impl Sql, order: i64, line: &LineRequest) -> ApiResult<i64> {
    price_of(sql, line.menu_item_id, &line.size)?;
    let known =
        sql.integer("SELECT count(*) FROM modifier WHERE id IN (SELECT value FROM json_each(?1))", &[json_text(&line.modifiers)?])?;
    let mut wanted = line.modifiers.clone();
    wanted.sort_unstable();
    wanted.dedup();
    if known != wanted.len() as i64 {
        return Err(ApiError::not_found(format!("one of the modifiers {:?}", line.modifiers)));
    }
    let rows =
        sql.rows(ADD_LINE, &[int(order), int(line.menu_item_id), text(&line.size), json_text(&line.modifiers)?, int(line.quantity)])?;
    let id = Record::first(&rows).map(|row| row.int("id")).ok_or_else(|| ApiError::internal("the line was not written"))?;
    sql.run(
        "INSERT INTO order_line_modifier (line_id, modifier_id, price_cents)
         SELECT ?1, id, price_cents FROM modifier WHERE id IN (SELECT value FROM json_each(?2))
         ON CONFLICT (line_id, modifier_id) DO NOTHING",
        &[int(id), json_text(&line.modifiers)?],
    )?;
    Ok(id)
}

/// Sets an order's promotion by its code.
///
/// The code is matched without regard to case, because `promotion.code` is
/// `COLLATE NOCASE`. An order that does not qualify on its day or its
/// subtotal is refused here; [`reprice`] also leaves the promotion out if the
/// order stops qualifying later, when a line is removed.
///
/// @param sql - the open transaction
/// @param order - the order
/// @param code - the code
fn set_promotion(sql: &impl Sql, order: i64, code: &str) -> ApiResult<()> {
    let rows = sql.rows(
        "SELECT p.id, o.business_day BETWEEN p.starts_on AND p.ends_on AS in_dates, p.starts_on, p.ends_on
         FROM promotion p, orders o WHERE p.code = ?1 AND o.id = ?2",
        &[text(code.trim()), int(order)],
    )?;
    let promotion = Record::first(&rows).ok_or_else(|| ApiError::not_found(format!("promotion code {code}")))?;
    if promotion.int("in_dates") == 0 {
        let (from, to) = (promotion.text("starts_on"), promotion.text("ends_on"));
        return Err(ApiError::conflict(format!("promotion {code} runs from {from} to {to}")));
    }
    sql.run("UPDATE orders SET promotion_id = ?2 WHERE id = ?1", &[int(order), int(promotion.int("id"))])?;
    Ok(())
}

/// Sets how many points an order spends. The `order_redeems_points_it_has`
/// trigger refuses more than the customer has, and a CHECK refuses a number
/// that is not a multiple of 100 or points on an order with no customer.
///
/// @param sql - the open transaction
/// @param order - the order
/// @param points - how many points
fn set_redeem(sql: &impl Sql, order: i64, points: i64) -> ApiResult<()> {
    sql.run("UPDATE orders SET redeem_points = ?2 WHERE id = ?1", &[int(order), int(points)])?;
    Ok(())
}

/// Writes an open order's subtotal, discount and tax.
///
/// Three statements, in order, because each reads the column the one before
/// it wrote. In one `UPDATE`, every `SET` expression sees the row as it was
/// before the statement, so the tax would be worked out from the old subtotal.
///
/// @param sql - the open transaction
/// @param order - the order
pub fn reprice(sql: &impl Sql, order: i64) -> ApiResult<()> {
    sql.run(
        "UPDATE orders SET subtotal_cents = (SELECT coalesce(sum(line_total_cents), 0) FROM order_line WHERE order_id = ?1)
         WHERE id = ?1 AND status = 'open'",
        &[int(order)],
    )?;
    sql.run(&format!("{PRICE_DISCOUNT} AND status = 'open'"), &[int(order)])?;
    sql.run(&format!("{PRICE_TAX} AND status = 'open'"), &[int(order)])?;
    Ok(())
}

/// Refuses a promotion the order's subtotal is too small for.
///
/// [`reprice`] leaves such a promotion out of the discount without an error,
/// because removing a line should not fail. Applying the code is where the
/// customer should hear that it does not apply yet.
///
/// @param sql - the open transaction
/// @param order - the order
fn check_promotion_qualifies(sql: &impl Sql, order: i64) -> ApiResult<()> {
    let promotion = sql.rows(
        "SELECT p.code, p.min_subtotal_cents, o.subtotal_cents FROM orders o JOIN promotion p ON p.id = o.promotion_id
         WHERE o.id = ?1 AND o.subtotal_cents < p.min_subtotal_cents AND o.status = 'open'",
        &[int(order)],
    )?;
    if let Some(row) = Record::first(&promotion) {
        let (code, needs, has) = (row.text("code"), row.int("min_subtotal_cents"), row.int("subtotal_cents"));
        return Err(ApiError::conflict(format!("promotion {code} needs a subtotal of {needs} cents, and the order has {has}")));
    }
    Ok(())
}

/// Answers `404` when an order does not exist.
///
/// @param sql - the open transaction
/// @param order - the order
fn require_order(sql: &impl Sql, order: i64) -> ApiResult<String> {
    let rows = sql.rows("SELECT status FROM orders WHERE id = ?1", &[int(order)])?;
    Record::first(&rows).map(|row| row.text("status")).ok_or_else(|| ApiError::not_found(format!("order {order}")))
}

/// Answers `404` when an order does not exist and `409` when it is not open.
///
/// @param sql - the open transaction
/// @param order - the order
pub fn require_open(sql: &impl Sql, order: i64) -> ApiResult<()> {
    match require_order(sql, order)?.as_str() {
        "open" => Ok(()),
        status => Err(ApiError::conflict(format!("order {order} is {status}, and only an open order can change"))),
    }
}
