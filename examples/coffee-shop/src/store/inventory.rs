//! Stock: ingredients, deliveries from suppliers, paying suppliers, waste,
//! stock counts, and the checks that the stock and the books agree.
//!
//! Every change to what is on the shelf is a row in `stock_movement`, with
//! what it cost. The `stock_movement_applies` trigger keeps `on_hand` equal to
//! the movements' sum, and every movement that changes the value of the stock
//! has a journal entry against the `1200 Inventory` account for the same
//! amount. [`Store::reconcile`] proves both.

use serde::Deserialize;
use serde_json::{json, Value as Json};

use super::{ensure_balanced, int, json_text, text, Record, Sql, Store};
use crate::error::{ApiError, ApiResult};

/// The body of `POST /ingredients`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewIngredient {
    /// Its name, unique ignoring case.
    pub name: String,
    /// `g`, `ml` or `each`.
    pub unit: String,
    /// Below this, it shows as low in `GET /inventory`.
    #[serde(default)]
    pub reorder_level: i64,
}

/// The body of `POST /purchases`: a delivery with its invoice.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewPurchase {
    /// Who delivered it.
    pub supplier: String,
    /// Their invoice number. A supplier's invoice can be entered once.
    pub invoice: String,
    /// When it arrived. Now when left out.
    #[serde(default)]
    pub at: Option<String>,
    /// What arrived.
    pub lines: Vec<PurchaseLine>,
}

/// One ingredient of a delivery.
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PurchaseLine {
    /// The ingredient.
    pub ingredient_id: i64,
    /// How much arrived, in the ingredient's unit.
    pub quantity: i64,
    /// What that quantity cost in total, in cents.
    pub cost_cents: i64,
}

/// The body of `POST /inventory/waste`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Waste {
    /// The ingredient.
    pub ingredient_id: i64,
    /// How much was thrown away.
    pub quantity: i64,
    /// Why, such as `milk out of date`.
    #[serde(default)]
    pub note: String,
    /// When. Now when left out.
    #[serde(default)]
    pub at: Option<String>,
}

/// The body of `POST /inventory/count`: what was on the shelf when counted.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Count {
    /// One count per ingredient counted.
    pub counts: Vec<CountLine>,
    /// When. Now when left out.
    #[serde(default)]
    pub at: Option<String>,
}

/// One ingredient's count.
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct CountLine {
    /// The ingredient.
    pub ingredient_id: i64,
    /// What is on the shelf.
    pub counted: i64,
}

/// Every ingredient with its stock, its value, what the last seven days used,
/// and how many days that stock lasts at that rate.
///
/// The list should put the low ones first with `ORDER BY low DESC, i.name`.
/// inillucent 1.0.30 refuses a query with a window function that is ordered
/// by an expression (task-2134 item 9), and `low` is one, so the query is
/// ordered by name and [`Store::inventory`] moves the low ones up.
const INVENTORY: &str = "
SELECT i.id, i.name, i.unit, i.on_hand, i.reorder_level, i.unit_cost_micros,
       (i.on_hand * i.unit_cost_micros + 5000) / 10000 AS value_cents,
       i.on_hand <= i.reorder_level AS low,
       coalesce(u.used, 0) AS used_last_7_days,
       CASE WHEN u.used > 0 THEN round(i.on_hand * 7.0 / u.used, 1) END AS days_of_cover,
       rank() OVER (ORDER BY i.on_hand * i.unit_cost_micros DESC) AS rank_by_value
FROM ingredient i
LEFT JOIN (SELECT ingredient_id, -sum(change) AS used
           FROM stock_movement
           WHERE reason = 'sale' AND at >= date(?1, '-7 days') AND at < date(?1, '+1 day')
           GROUP BY ingredient_id) u ON u.ingredient_id = i.id
ORDER BY i.name";

/// A purchase moves each ingredient's average cost toward what it paid:
/// (what was on hand at the old cost + what arrived at its cost) divided by
/// the new quantity. Stock that went below zero counts as none.
const AVERAGE_COST: &str = "
UPDATE ingredient
SET unit_cost_micros = (max(ingredient.on_hand, 0) * ingredient.unit_cost_micros + pl.cost_cents * 10000)
                       / (max(ingredient.on_hand, 0) + pl.quantity)
FROM purchase_line pl
WHERE pl.purchase_id = ?1 AND pl.ingredient_id = ingredient.id";

impl Store {
    /// `POST /ingredients`.
    ///
    /// @param ingredient - the new ingredient
    pub fn create_ingredient(&self, ingredient: &NewIngredient) -> ApiResult<Json> {
        let id = self.integer(
            "INSERT INTO ingredient (name, unit, reorder_level) VALUES (trim(?1), ?2, ?3) RETURNING id",
            &[text(&ingredient.name), text(&ingredient.unit), int(ingredient.reorder_level)],
        )?;
        self.object("SELECT * FROM ingredient WHERE id = ?1", &[int(id)], "ingredient")
    }

    /// `GET /inventory`: every ingredient, the low ones first.
    ///
    /// @param day - the last day of the seven used to measure what is used
    pub fn inventory(&self, day: &str) -> ApiResult<Vec<Json>> {
        let mut rows = self.objects(INVENTORY, &[text(day)], &[])?;
        rows.sort_by_key(|row| row["low"].as_i64() != Some(1));
        Ok(rows)
    }

    /// `GET /inventory/{id}/movements`: every movement of one ingredient, with
    /// the stock after each one from a running `sum()`.
    ///
    /// @param id - the ingredient
    pub fn movements(&self, id: i64) -> ApiResult<Vec<Json>> {
        self.objects(
            "SELECT m.id, m.at, m.reason, m.change, m.cost_cents, o.ticket, p.invoice, m.note,
                    sum(m.change) OVER (ORDER BY m.id) AS on_hand_after
             FROM stock_movement m
             LEFT JOIN orders o ON o.id = m.order_id
             LEFT JOIN purchase p ON p.id = m.purchase_id
             WHERE m.ingredient_id = ?1
             ORDER BY m.id",
            &[int(id)],
            &[],
        )
    }

    /// `POST /purchases`: receives a delivery.
    ///
    /// In one transaction: the purchase and its lines, the new average costs
    /// (before the stock arrives, because the formula needs the old
    /// quantity), a stock movement per line, and the journal entry that puts
    /// the stock on the books and the invoice in accounts payable.
    ///
    /// @param purchase - the delivery
    pub fn receive_purchase(&self, purchase: &NewPurchase) -> ApiResult<Json> {
        if purchase.lines.is_empty() {
            return Err(ApiError::bad_request("a purchase needs at least one line"));
        }
        let at = self.timestamp(purchase.at.as_deref())?;
        let lines = json_text(&purchase.lines)?;
        let tx = self.begin()?;
        let id = tx.integer(
            "INSERT INTO purchase (supplier, invoice, received_at, total_cents)
             SELECT trim(?1), trim(?2), ?3, sum(value ->> '$.cost_cents') FROM json_each(?4)
             RETURNING id",
            &[text(&purchase.supplier), text(&purchase.invoice), text(&at), lines.clone()],
        )?;
        tx.run(
            "INSERT INTO purchase_line (purchase_id, ingredient_id, quantity, cost_cents)
             SELECT ?1, value ->> '$.ingredient_id', value ->> '$.quantity', value ->> '$.cost_cents' FROM json_each(?2)",
            &[int(id), lines],
        )?;
        tx.run(AVERAGE_COST, &[int(id)])?;
        tx.run(
            "INSERT INTO stock_movement (ingredient_id, change, cost_cents, reason, purchase_id, at)
             SELECT ingredient_id, quantity, cost_cents, 'purchase', purchase_id, ?2
             FROM purchase_line WHERE purchase_id = ?1",
            &[int(id), text(&at)],
        )?;
        let total = tx.integer("SELECT total_cents FROM purchase WHERE id = ?1", &[int(id)])?;
        let memo = format!("delivery from {}, invoice {}", purchase.supplier.trim(), purchase.invoice.trim());
        post(&tx, &at, "purchase", Some(id), &memo, &[("1200", total, 0), ("2200", 0, total)])?;
        ensure_balanced(&tx)?;
        tx.commit()?;
        self.purchase(id)
    }

    /// `GET /purchases/{id}`: a delivery with its lines.
    ///
    /// @param id - the purchase
    pub fn purchase(&self, id: i64) -> ApiResult<Json> {
        let mut purchase = self.object("SELECT * FROM purchase WHERE id = ?1", &[int(id)], &format!("purchase {id}"))?;
        purchase["lines"] = Json::from(self.objects(
            "SELECT pl.ingredient_id, i.name, pl.quantity, i.unit, pl.cost_cents,
                    pl.cost_cents * 10000 / pl.quantity AS unit_cost_micros
             FROM purchase_line pl JOIN ingredient i ON i.id = pl.ingredient_id
             WHERE pl.purchase_id = ?1 ORDER BY i.name",
            &[int(id)],
            &[],
        )?);
        Ok(purchase)
    }

    /// `POST /purchases/{id}/pay`: pays the supplier from the bank.
    ///
    /// The journal entry's `UNIQUE (source, source_id)` is what stops an
    /// invoice being paid twice; the `UPDATE` only records when.
    ///
    /// @param id - the purchase
    /// @param at - when, or now
    pub fn pay_supplier(&self, id: i64, at: Option<&str>) -> ApiResult<Json> {
        let at = self.timestamp(at)?;
        let tx = self.begin()?;
        let rows = tx.rows("SELECT total_cents, supplier, invoice FROM purchase WHERE id = ?1", &[int(id)])?;
        let row = Record::first(&rows).ok_or_else(|| ApiError::not_found(format!("purchase {id}")))?;
        let (total, memo) = (row.int("total_cents"), format!("paid {}, invoice {}", row.text("supplier"), row.text("invoice")));
        post(&tx, &at, "supplier_payment", Some(id), &memo, &[("2200", total, 0), ("1020", 0, total)])?;
        tx.run("UPDATE purchase SET paid_at = ?2 WHERE id = ?1", &[int(id), text(&at)])?;
        ensure_balanced(&tx)?;
        tx.commit()?;
        self.purchase(id)
    }

    /// `POST /inventory/waste`: stock thrown away, written off at its average cost.
    ///
    /// @param waste - what, how much and why
    pub fn waste(&self, waste: &Waste) -> ApiResult<Json> {
        let at = self.timestamp(waste.at.as_deref())?;
        let tx = self.begin()?;
        let movement = tx.integer(
            "INSERT INTO stock_movement (ingredient_id, change, cost_cents, reason, note, at)
             SELECT id, -?2, -((?2 * unit_cost_micros + 5000) / 10000), 'waste', ?3, ?4 FROM ingredient WHERE id = ?1
             RETURNING id",
            &[int(waste.ingredient_id), int(waste.quantity), text(&waste.note), text(&at)],
        )?;
        if movement == 0 {
            return Err(ApiError::not_found(format!("ingredient {}", waste.ingredient_id)));
        }
        let cost = -tx.integer("SELECT cost_cents FROM stock_movement WHERE id = ?1", &[int(movement)])?;
        if cost > 0 {
            let memo = format!("waste: {}", waste.note);
            post(&tx, &at, "waste", Some(movement), &memo, &[("5100", cost, 0), ("1200", 0, cost)])?;
        }
        ensure_balanced(&tx)?;
        tx.commit()?;
        self.object("SELECT * FROM stock_movement WHERE id = ?1", &[int(movement)], "movement")
    }

    /// `POST /inventory/count`: sets the stock to what was counted.
    ///
    /// One `INSERT ... SELECT` joins the counts to the ingredients and writes a
    /// movement for each one that differs, valued at its average cost. The
    /// counts are read out of the JSON in a derived table first, and joined
    /// by its plain columns: inillucent 1.0.30 refuses a join whose condition
    /// reads `json_each.value ->> '$.field'` directly (task-2134 item 6). The
    /// difference in value goes to `5150 Stock count changes`, on whichever
    /// side the sign puts it.
    ///
    /// @param count - the counts
    pub fn count(&self, count: &Count) -> ApiResult<Json> {
        let at = self.timestamp(count.at.as_deref())?;
        let tx = self.begin()?;
        let first = tx.integer("SELECT coalesce(max(id), 0) + 1 FROM stock_movement", &[])?;
        tx.run(
            "INSERT INTO stock_movement (ingredient_id, change, cost_cents, reason, note, at)
             SELECT i.id, c.counted - i.on_hand,
                    CASE WHEN c.counted > i.on_hand THEN ((c.counted - i.on_hand) * i.unit_cost_micros + 5000) / 10000
                         ELSE -(((i.on_hand - c.counted) * i.unit_cost_micros + 5000) / 10000) END,
                    'count', 'counted ' || c.counted || ', expected ' || i.on_hand, ?2
             FROM (SELECT value ->> '$.ingredient_id' AS ingredient_id, value ->> '$.counted' AS counted FROM json_each(?1)) c
             JOIN ingredient i ON i.id = c.ingredient_id
             WHERE c.counted <> i.on_hand",
            &[json_text(&count.counts)?, text(&at)],
        )?;
        let value = tx.integer("SELECT coalesce(sum(cost_cents), 0) FROM stock_movement WHERE id >= ?1", &[int(first)])?;
        let lines = if value >= 0 { [("1200", value, 0), ("5150", 0, value)] } else { [("5150", -value, 0), ("1200", 0, -value)] };
        post(&tx, &at, "count", Some(first), "stock count", &lines)?;
        ensure_balanced(&tx)?;
        let movements = tx.rows("SELECT id FROM stock_movement WHERE id >= ?1", &[int(first)])?.rows.len();
        tx.commit()?;
        Ok(json!({ "movements": movements, "value_change_cents": value }))
    }

    /// `GET /ledger/check`: the checks that the books, the stock and the
    /// ingredient table agree. Every list in the answer should be empty.
    pub fn reconcile(&self) -> ApiResult<Json> {
        let stock_mismatches = self.objects(
            "SELECT id AS ingredient_id, on_hand FROM ingredient
             EXCEPT
             SELECT i.id, coalesce(sum(m.change), 0) FROM ingredient i LEFT JOIN stock_movement m ON m.ingredient_id = i.id
             GROUP BY i.id",
            &[],
            &[],
        )?;
        let unbalanced = self.objects("SELECT * FROM unbalanced_entry", &[], &[])?;
        let inventory = self.object(
            "SELECT (SELECT balance_cents FROM account_balance WHERE code = '1200') AS ledger_cents,
                    (SELECT coalesce(sum(cost_cents), 0) FROM stock_movement) AS movements_cents,
                    (SELECT coalesce(sum((on_hand * unit_cost_micros + 5000) / 10000), 0) FROM ingredient) AS valuation_cents",
            &[],
            "inventory",
        )?;
        let books_agree = inventory["ledger_cents"] == inventory["movements_cents"];
        Ok(json!({
            "ok": stock_mismatches.is_empty() && unbalanced.is_empty() && books_agree,
            "stock_mismatches": stock_mismatches,
            "unbalanced_entries": unbalanced,
            "inventory": inventory,
        }))
    }
}

/// Posts a journal entry with its lines, and answers its id.
///
/// Lines with no amount are left out, because a journal line must have an
/// amount on one side. An entry whose lines are all zero, such as a deposit
/// on a day with no cash, is not written at all, and the answer is 0.
///
/// @param sql - the open transaction
/// @param at - when; the entry's business day is its date
/// @param source - what wrote it, one of the values `journal_entry.source` allows
/// @param source_id - the row it is about
/// @param memo - what it is, in words
/// @param lines - `(account, debit, credit)` for each line
pub fn post(sql: &impl Sql, at: &str, source: &str, source_id: Option<i64>, memo: &str, lines: &[(&str, i64, i64)]) -> ApiResult<i64> {
    if lines.iter().all(|(_, debit, credit)| *debit == 0 && *credit == 0) {
        return Ok(0);
    }
    let entry = sql.integer(
        "INSERT INTO journal_entry (business_day, posted_at, source, source_id, memo) VALUES (date(?1), ?1, ?2, ?3, ?4) RETURNING id",
        &[text(at), text(source), super::opt_int(source_id), text(memo)],
    )?;
    let lines: Vec<Json> = lines.iter().map(|(account, debit, credit)| json!([account, debit, credit])).collect();
    sql.run(
        "INSERT INTO journal_line (entry_id, account_code, debit_cents, credit_cents)
         SELECT ?1, value ->> '$[0]', value ->> '$[1]', value ->> '$[2]' FROM json_each(?2)
         WHERE value ->> '$[1]' > 0 OR value ->> '$[2]' > 0
         ORDER BY key",
        &[int(entry), json_text(&lines)?],
    )?;
    Ok(entry)
}
