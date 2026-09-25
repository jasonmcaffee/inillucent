//! The books: manual journal entries, each account's ledger, and closing a
//! business day.
//!
//! Closing a day is the end of the till's day. It shares the tips among the
//! staff who worked, counts the cash drawer against what the books say should
//! be in it, banks everything above the float, and settles the day's card
//! payments less the card fee. Each of those is a journal entry, all in one
//! transaction, and the `day_close` row it writes last stops any more orders
//! or payments on that day.

use serde::Deserialize;
use serde_json::Value as Json;

use super::inventory::post;
use super::{ensure_balanced, int, json_text, text, Record, Sql, Store};
use crate::error::{ApiError, ApiResult};

/// The body of `POST /journal`: an entry a person makes by hand, such as the
/// owner putting money into the business.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualEntry {
    /// What it is.
    pub memo: String,
    /// When. Now when left out.
    #[serde(default)]
    pub at: Option<String>,
    /// The lines. Their debits must equal their credits.
    pub lines: Vec<ManualLine>,
}

/// One line of a manual entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualLine {
    /// The account code, such as `1020`.
    pub account: String,
    /// The debit, in cents.
    #[serde(default)]
    pub debit_cents: i64,
    /// The credit, in cents.
    #[serde(default)]
    pub credit_cents: i64,
}

/// The body of `POST /days/{day}/close`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseDay {
    /// The cash counted in the drawer, float included.
    pub counted_cash_cents: i64,
    /// When. Now when left out.
    #[serde(default)]
    pub at: Option<String>,
}

/// The day's tips shared among the staff by the minutes each worked, to the
/// cent, with no cent lost or made up.
///
/// Each person gets the whole cents of their share (`base`). That leaves a
/// few cents over, fewer than there are people. The people with the largest
/// fractions left over (`remainder`) get one cent each until the pool is
/// used up: `row_number()` orders them, and `sum(base) OVER ()` is the total
/// already handed out. This is the largest remainder method, and it runs as
/// one query. The CTEs hold no window functions, because inillucent 1.0.30
/// refuses a window function inside a CTE (task-2132 item 6); the window
/// functions are all in the outer `SELECT`, where they work.
const TIP_POOL: &str = "
WITH worked AS (
  SELECT staff_id, sum((unixepoch(ended_at) - unixepoch(started_at)) / 60) AS minutes
  FROM shift
  WHERE date(started_at) = ?1 AND ended_at IS NOT NULL
  GROUP BY staff_id
),
pool AS (
  SELECT coalesce(sum(tip_cents), 0) AS cents FROM payment WHERE business_day = ?1
),
share AS (
  SELECT w.staff_id, w.minutes,
         pool.cents * w.minutes / (SELECT sum(minutes) FROM worked) AS base,
         pool.cents * w.minutes % (SELECT sum(minutes) FROM worked) AS remainder
  FROM worked w, pool
)
SELECT share.staff_id, s.name, share.minutes AS minutes_worked,
       share.base + CASE WHEN row_number() OVER (ORDER BY share.remainder DESC, share.staff_id)
                              <= (SELECT cents FROM pool) - sum(share.base) OVER ()
                         THEN 1 ELSE 0 END AS tip_cents
FROM share JOIN staff s ON s.id = share.staff_id
ORDER BY share.staff_id";

/// An account's lines between two days, with the balance after each one.
///
/// The balance starts from the account's balance before the first day, bound
/// as `?4`, and adds a running `sum()` of each line on the account's normal
/// side.
const ACCOUNT_LEDGER: &str = "
SELECT e.id AS entry_id, e.business_day, e.posted_at, e.source, e.memo, l.debit_cents, l.credit_cents,
       ?4 + sum(CASE WHEN a.type IN ('asset', 'expense') THEN l.debit_cents - l.credit_cents
                     ELSE l.credit_cents - l.debit_cents END)
            OVER (ORDER BY e.business_day, e.id, l.id ROWS UNBOUNDED PRECEDING) AS balance_cents
FROM journal_line l
JOIN journal_entry e ON e.id = l.entry_id
JOIN account a ON a.code = l.account_code
WHERE l.account_code = ?1 AND e.business_day BETWEEN ?2 AND ?3
ORDER BY e.business_day, e.id, l.id";

/// An account's balance on its normal side at the end of a day.
const BALANCE_ON: &str = "
SELECT coalesce(sum(CASE WHEN a.type IN ('asset', 'expense') THEN l.debit_cents - l.credit_cents
                         ELSE l.credit_cents - l.debit_cents END), 0)
FROM journal_line l
JOIN journal_entry e ON e.id = l.entry_id
JOIN account a ON a.code = l.account_code
WHERE l.account_code = ?1 AND e.business_day <= ?2";

impl Store {
    /// `POST /journal`: a manual entry.
    ///
    /// A person's entry that does not balance is their mistake, so it is
    /// answered with `409` before anything is written. The `unbalanced_entry`
    /// check that follows is for this program's own mistakes.
    ///
    /// @param entry - the entry
    pub fn manual_entry(&self, entry: &ManualEntry) -> ApiResult<Json> {
        let debits: i64 = entry.lines.iter().map(|line| line.debit_cents).sum();
        let credits: i64 = entry.lines.iter().map(|line| line.credit_cents).sum();
        if debits != credits || debits == 0 {
            return Err(ApiError::conflict(format!("the debits come to {debits} cents and the credits to {credits}; they must be equal")));
        }
        let at = self.timestamp(entry.at.as_deref())?;
        let lines: Vec<(&str, i64, i64)> =
            entry.lines.iter().map(|line| (line.account.as_str(), line.debit_cents, line.credit_cents)).collect();
        let tx = self.begin()?;
        let id = post(&tx, &at, "manual", None, entry.memo.trim(), &lines)?;
        ensure_balanced(&tx)?;
        tx.commit()?;
        self.entry(id)
    }

    /// `GET /journal/{id}`: one entry and its lines.
    ///
    /// @param id - the entry
    pub fn entry(&self, id: i64) -> ApiResult<Json> {
        let mut entry = self.object("SELECT * FROM journal_entry WHERE id = ?1", &[int(id)], &format!("journal entry {id}"))?;
        entry["lines"] = Json::from(self.objects(
            "SELECT l.account_code, a.name AS account, l.debit_cents, l.credit_cents
             FROM journal_line l JOIN account a ON a.code = l.account_code WHERE l.entry_id = ?1 ORDER BY l.id",
            &[int(id)],
            &[],
        )?);
        Ok(entry)
    }

    /// `GET /journal`: the entries of a day, each with its lines.
    ///
    /// @param day - the business day
    pub fn journal(&self, day: &str) -> ApiResult<Vec<Json>> {
        let ids = self.rows("SELECT id FROM journal_entry WHERE business_day = ?1 ORDER BY id", &[text(day)])?;
        Record::all(&ids).map(|row| self.entry(row.int("id"))).collect()
    }

    /// `GET /accounts`: every account with its balance.
    pub fn accounts(&self) -> ApiResult<Vec<Json>> {
        self.objects("SELECT * FROM account_balance ORDER BY code", &[], &[])
    }

    /// `GET /accounts/{code}/ledger`: one account's lines with a running balance.
    ///
    /// @param code - the account
    /// @param from - the first day
    /// @param to - the last day
    pub fn account_ledger(&self, code: &str, from: &str, to: &str) -> ApiResult<Json> {
        let mut account = self.object("SELECT * FROM account WHERE code = ?1", &[text(code)], &format!("account {code}"))?;
        let opening = self.integer(BALANCE_ON, &[text(code), text(&self.day_after(from, -1)?)])?;
        account["opening_balance_cents"] = Json::from(opening);
        account["lines"] = Json::from(self.objects(ACCOUNT_LEDGER, &[text(code), text(from), text(to), int(opening)], &[])?);
        account["closing_balance_cents"] = Json::from(self.integer(BALANCE_ON, &[text(code), text(to)])?);
        Ok(account)
    }

    /// `GET /reports/tips`: how a day's tips would be shared, before the close.
    ///
    /// @param day - the business day
    pub fn tip_pool(&self, day: &str) -> ApiResult<Vec<Json>> {
        self.objects(TIP_POOL, &[text(day)], &[])
    }

    /// `POST /days/{day}/close`: closes a business day.
    ///
    /// Refused while the day has open orders, or once it is closed. Then, in
    /// order, each step posting to the books:
    ///
    /// 1. the tips are shared and paid out from the drawer;
    /// 2. the cash counted is compared with the drawer's balance in the books,
    ///    and any difference is written to `5300 Cash over and short`;
    /// 3. everything above the float goes to the bank;
    /// 4. the day's card payments are settled to the bank, less the fee.
    ///
    /// @param day - the business day
    /// @param close - the cash counted
    pub fn close_day(&self, day: &str, close: &CloseDay) -> ApiResult<Json> {
        let at = self.timestamp(close.at.as_deref())?;
        let tx = self.begin()?;
        refuse_if_closed_or_open(&tx, day)?;
        let source = tx.integer("SELECT CAST(strftime('%Y%m%d', ?1) AS INTEGER)", &[text(day)])?;
        let tips = pay_out_tips(&tx, day, &at, source)?;
        let expected = tx.integer(BALANCE_ON, &[text("1000"), text(day)])?;
        let over = close.counted_cash_cents - expected;
        let memo = format!("cash count, {day}");
        match over {
            0 => 0,
            over if over > 0 => post(&tx, &at, "cash_count", Some(source), &memo, &[("1000", over, 0), ("5300", 0, over)])?,
            short => post(&tx, &at, "cash_count", Some(source), &memo, &[("5300", -short, 0), ("1000", 0, -short)])?,
        };
        let deposit = (close.counted_cash_cents - tx.setting("drawer_float_cents")?).max(0);
        post(&tx, &at, "deposit", Some(source), &format!("bank deposit, {day}"), &[("1020", deposit, 0), ("1000", 0, deposit)])?;
        let card = tx.integer(BALANCE_ON, &[text("1010"), text(day)])?;
        let fee = (card * tx.setting("card_fee_bp")? + 5000) / 10000;
        let settle = [("1020", card - fee, 0), ("5200", fee, 0), ("1010", 0, card)];
        post(&tx, &at, "card_settlement", Some(source), &format!("card settlement, {day}"), &settle)?;
        tx.run(
            "INSERT INTO day_close (business_day, closed_at, counted_cash_cents, expected_cash_cents, deposit_cents,
                                    card_settled_cents, card_fee_cents)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            &[text(day), text(&at), int(close.counted_cash_cents), int(expected), int(deposit), int(card), int(fee)],
        )?;
        tx.run(
            "INSERT INTO tip_payout (business_day, staff_id, minutes_worked, tip_cents)
             SELECT ?1, value ->> '$.staff_id', value ->> '$.minutes_worked', value ->> '$.tip_cents' FROM json_each(?2)",
            &[text(day), json_text(&tips)?],
        )?;
        ensure_balanced(&tx)?;
        tx.commit()?;
        self.z_report(day)
    }
}

/// Refuses to close a day that is closed, or that still has open orders.
///
/// @param sql - the open transaction
/// @param day - the business day
fn refuse_if_closed_or_open(sql: &impl Sql, day: &str) -> ApiResult<()> {
    if sql.integer("SELECT count(*) FROM day_close WHERE business_day = ?1", &[text(day)])? > 0 {
        return Err(ApiError::conflict(format!("{day} is already closed")));
    }
    let open = sql.rows(
        "SELECT group_concat(ticket, ', ' ORDER BY ticket) AS tickets FROM orders WHERE business_day = ?1 AND status = 'open'",
        &[text(day)],
    )?;
    match Record::first(&open).and_then(|row| row.opt_text("tickets")) {
        Some(tickets) => Err(ApiError::conflict(format!("{day} still has open orders: tickets {tickets}. Pay or cancel them first"))),
        None => Ok(()),
    }
}

/// Shares the day's tips and posts paying them out of the drawer.
///
/// When nobody has a finished shift on the day, or refunds took back more
/// tips than the day took, nothing is paid and the tips stay in `2100 Tips
/// payable`.
///
/// @param sql - the open transaction
/// @param day - the business day
/// @param at - when the day was closed
/// @param source - the day as a number, for the journal entry's source row
fn pay_out_tips(sql: &impl Sql, day: &str, at: &str, source: i64) -> ApiResult<Vec<Json>> {
    let shares = sql.objects(TIP_POOL, &[text(day)], &[])?;
    let paid: i64 = shares.iter().filter_map(|share| share["tip_cents"].as_i64()).sum();
    if paid <= 0 {
        return Ok(Vec::new());
    }
    post(sql, at, "tips", Some(source), &format!("tips paid out, {day}"), &[("2100", paid, 0), ("1000", 0, paid)])?;
    Ok(shares)
}
