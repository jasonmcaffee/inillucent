//! Reports: the end of day Z report, sales by day and by hour, what sells,
//! and the three statements an accountant asks for.
//!
//! Every report is a query whose rows are the answer. The money reports read
//! the books, `journal_line`, and not the orders, so they agree with each
//! other by construction. The Z report reads both, and its `books` section is
//! where a difference between the till and the ledger would show.

use serde_json::{json, Value as Json};

use super::{text, Sql, Store};
use crate::error::{ApiError, ApiResult};

/// The day's orders, counted by status and summed, in one scan with `FILTER`.
const Z_ORDERS: &str = "
SELECT count(*) FILTER (WHERE paid_at IS NOT NULL) AS orders_paid,
       count(*) FILTER (WHERE status = 'refunded') AS orders_refunded,
       count(*) FILTER (WHERE status = 'cancelled') AS orders_cancelled,
       count(*) FILTER (WHERE status = 'open') AS orders_open,
       coalesce(sum(subtotal_cents) FILTER (WHERE paid_at IS NOT NULL), 0) AS gross_sales_cents,
       coalesce(sum(discount_cents) FILTER (WHERE paid_at IS NOT NULL), 0) AS discounts_cents,
       coalesce(sum(tax_cents) FILTER (WHERE paid_at IS NOT NULL), 0) AS tax_cents,
       coalesce(sum(total_cents) FILTER (WHERE paid_at IS NOT NULL), 0) AS takings_cents,
       CAST(round(avg(total_cents) FILTER (WHERE paid_at IS NOT NULL)) AS INTEGER) AS average_ticket_cents,
       min(opened_at) AS first_order_at,
       max(paid_at) AS last_payment_at
FROM orders WHERE business_day = ?1";

/// The day's money in and out, by method, from the payments.
const Z_PAYMENTS: &str = "
SELECT coalesce(sum(amount_cents) FILTER (WHERE method = 'cash' AND refund_of IS NULL), 0) AS cash_cents,
       coalesce(sum(amount_cents) FILTER (WHERE method = 'card' AND refund_of IS NULL), 0) AS card_cents,
       coalesce(sum(tip_cents) FILTER (WHERE refund_of IS NULL), 0) AS tips_cents,
       coalesce(-sum(amount_cents + tip_cents) FILTER (WHERE refund_of IS NOT NULL), 0) AS refunded_cents,
       count(*) FILTER (WHERE refund_of IS NULL) AS payments
FROM payment WHERE business_day = ?1";

/// The day's five best sellers by how many were sold.
///
/// The count is named `sold` and not `quantity`, because `order_line` has a
/// column called `quantity`. SQLite sorts `ORDER BY quantity` by the alias;
/// inillucent 1.0.30 sorts by the table's column, one row of each group, and
/// the list comes out in the wrong order (task-2134 item 7).
const TOP_ITEMS: &str = "
SELECT m.name, sum(l.quantity) AS sold, sum(l.line_total_cents) AS revenue_cents
FROM order_line l
JOIN orders o ON o.id = l.order_id
JOIN menu_item m ON m.id = l.menu_item_id
WHERE o.business_day = ?1 AND o.paid_at IS NOT NULL
GROUP BY m.id
ORDER BY sold DESC, m.name
LIMIT 5";

/// What the day's entries did to each account.
const Z_BOOKS: &str = "
SELECT a.code, a.name, sum(l.debit_cents) AS debit_cents, sum(l.credit_cents) AS credit_cents
FROM journal_line l
JOIN journal_entry e ON e.id = l.entry_id
JOIN account a ON a.code = l.account_code
WHERE e.business_day = ?1
GROUP BY a.code
ORDER BY a.code";

/// Net sales for every day in a range, including the days with none.
///
/// The recursive CTE `calendar` makes one row per day, so a day with no sales
/// is a row of zeros, not a missing row. Sales count on the day they were
/// paid and refunds on the day they were refunded, which is what the books
/// do, so the two are stacked with `UNION ALL` and summed per day. Then
/// `lag()` compares each day with the one before, a running `sum()` totals the
/// range so far, and a frame of `ROWS BETWEEN 6 PRECEDING AND CURRENT ROW`
/// averages the last seven days.
const SALES_BY_DAY: &str = "
WITH RECURSIVE calendar (day) AS (
  SELECT ?1
  UNION ALL
  SELECT date(day, '+1 day') FROM calendar WHERE day < ?2
),
movements (day, orders, net_cents, refunds_cents) AS (
  SELECT date(paid_at), 1, subtotal_cents - discount_cents, 0 FROM orders WHERE paid_at IS NOT NULL
  UNION ALL
  SELECT date(refunded_at), 0, -(subtotal_cents - discount_cents), subtotal_cents - discount_cents
  FROM orders WHERE refunded_at IS NOT NULL
),
daily AS (
  SELECT day, sum(orders) AS orders, sum(net_cents) AS net_cents, sum(refunds_cents) AS refunds_cents
  FROM movements WHERE day BETWEEN ?1 AND ?2 GROUP BY day
)
SELECT c.day,
       CASE strftime('%w', c.day) WHEN '0' THEN 'Sun' WHEN '1' THEN 'Mon' WHEN '2' THEN 'Tue' WHEN '3' THEN 'Wed'
            WHEN '4' THEN 'Thu' WHEN '5' THEN 'Fri' ELSE 'Sat' END AS weekday,
       coalesce(d.orders, 0) AS orders,
       coalesce(d.net_cents, 0) AS net_sales_cents,
       coalesce(d.refunds_cents, 0) AS refunds_cents,
       coalesce(d.net_cents, 0) - lag(coalesce(d.net_cents, 0)) OVER by_day AS change_cents,
       sum(coalesce(d.net_cents, 0)) OVER by_day AS running_cents,
       CAST(round(avg(coalesce(d.net_cents, 0)) OVER (ORDER BY c.day ROWS BETWEEN 6 PRECEDING AND CURRENT ROW)) AS INTEGER)
         AS average_7_days_cents
FROM calendar c
LEFT JOIN daily d ON d.day = c.day
WINDOW by_day AS (ORDER BY c.day)
ORDER BY c.day";

/// Orders and sales for each opening hour of one day, with a text bar
/// scaled to the busiest hour by `max() OVER ()`.
const SALES_BY_HOUR: &str = "
WITH RECURSIVE hours (hour) AS (
  SELECT 6 UNION ALL SELECT hour + 1 FROM hours WHERE hour < 18
),
paid AS (
  SELECT CAST(strftime('%H', paid_at) AS INTEGER) AS hour, count(*) AS orders, sum(total_cents) AS takings_cents
  FROM orders WHERE date(paid_at) = ?1 GROUP BY 1
)
SELECT printf('%02d:00', h.hour) AS hour,
       coalesce(p.orders, 0) AS orders,
       coalesce(p.takings_cents, 0) AS takings_cents,
       round(100.0 * coalesce(p.takings_cents, 0) / nullif(sum(coalesce(p.takings_cents, 0)) OVER (), 0), 1) AS percent_of_day,
       substr('##############################', 1,
              CAST(round(30.0 * coalesce(p.orders, 0) / nullif(max(coalesce(p.orders, 0)) OVER (), 0)) AS INTEGER)) AS bar
FROM hours h
LEFT JOIN paid p ON p.hour = h.hour
ORDER BY h.hour";

/// Every item sold in a range, ranked by revenue across the menu and within
/// its category, with its share of sales and the running share from the top.
/// The running share is a Pareto line: how few items make most of the sales.
const ITEMS: &str = "
SELECT c.name AS category, m.sku, m.name,
       sum(l.quantity) AS quantity,
       sum(l.line_total_cents) AS revenue_cents,
       rank() OVER (ORDER BY sum(l.line_total_cents) DESC) AS rank,
       rank() OVER (PARTITION BY c.id ORDER BY sum(l.line_total_cents) DESC) AS rank_in_category,
       round(100.0 * sum(l.line_total_cents) / sum(sum(l.line_total_cents)) OVER (), 1) AS percent_of_sales,
       round(100.0 * sum(sum(l.line_total_cents)) OVER (ORDER BY sum(l.line_total_cents) DESC, m.sku ROWS UNBOUNDED PRECEDING)
             / sum(sum(l.line_total_cents)) OVER (), 1) AS cumulative_percent
FROM order_line l
JOIN orders o ON o.id = l.order_id
JOIN menu_item m ON m.id = l.menu_item_id
JOIN category c ON c.id = m.category_id
WHERE o.status IN ('paid', 'fulfilled') AND o.business_day BETWEEN ?1 AND ?2
GROUP BY m.id
ORDER BY rank, m.sku";

/// The items on the menu that sold nothing in a range: the menu minus what sold.
const UNSOLD: &str = "
SELECT sku, name FROM menu_item WHERE active = 1
EXCEPT
SELECT m.sku, m.name
FROM order_line l JOIN orders o ON o.id = l.order_id JOIN menu_item m ON m.id = l.menu_item_id
WHERE o.status IN ('paid', 'fulfilled') AND o.business_day BETWEEN ?1 AND ?2
ORDER BY sku";

/// Each account's balance at the end of a day, on its normal side. The three
/// statements below start from this.
const BALANCES: &str = "
balances AS (
  SELECT a.code, a.name, a.type,
         sum(l.debit_cents) AS debit_cents, sum(l.credit_cents) AS credit_cents,
         sum(CASE WHEN a.type IN ('asset', 'expense') THEN l.debit_cents - l.credit_cents
                  ELSE l.credit_cents - l.debit_cents END) AS balance_cents
  FROM account a
  JOIN journal_line l ON l.account_code = a.code
  JOIN journal_entry e ON e.id = l.entry_id
  WHERE e.business_day BETWEEN ?1 AND ?2
  GROUP BY a.code
)";

/// The trial balance: every account's net debit or credit, and a total row
/// added with `UNION ALL`. The two totals are equal when the books balance.
const TRIAL_BALANCE: &str = "
SELECT code, name, type,
       max(debit_cents - credit_cents, 0) AS debit_cents,
       max(credit_cents - debit_cents, 0) AS credit_cents
FROM balances
UNION ALL
SELECT 'total', '', '', sum(max(debit_cents - credit_cents, 0)), sum(max(credit_cents - debit_cents, 0))
FROM balances
ORDER BY code";

/// The income statement's lines: revenue and expense accounts in a range.
const INCOME_LINES: &str = "
SELECT type, code, name, balance_cents AS amount_cents FROM balances
WHERE type IN ('revenue', 'expense') AND balance_cents <> 0
ORDER BY code";

/// The income statement's totals, from the same rows, with `FILTER`.
const INCOME_TOTALS: &str = "
SELECT coalesce(sum(balance_cents) FILTER (WHERE code = '4000'), 0) AS sales_cents,
       coalesce(sum(balance_cents) FILTER (WHERE code = '4100'), 0) AS discounts_cents,
       coalesce(sum(balance_cents) FILTER (WHERE type = 'revenue'), 0) AS net_revenue_cents,
       coalesce(sum(balance_cents) FILTER (WHERE code = '5000'), 0) AS cost_of_goods_cents,
       coalesce(sum(balance_cents) FILTER (WHERE type = 'revenue'), 0)
         - coalesce(sum(balance_cents) FILTER (WHERE code = '5000'), 0) AS gross_profit_cents,
       coalesce(sum(balance_cents) FILTER (WHERE type = 'expense' AND code <> '5000'), 0) AS other_expenses_cents,
       coalesce(sum(balance_cents) FILTER (WHERE type = 'revenue'), 0)
         - coalesce(sum(balance_cents) FILTER (WHERE type = 'expense'), 0) AS net_income_cents
FROM balances";

/// The balance sheet's lines: assets, liabilities and equity, with the
/// earnings so far added to equity as one more line. Revenue less expenses
/// belongs to the owner, and until a period is closed it sits outside
/// `3000 Owner equity`.
const BALANCE_LINES: &str = "
SELECT type, code, name, balance_cents FROM balances
WHERE type IN ('asset', 'liability', 'equity') AND balance_cents <> 0
UNION ALL
SELECT 'equity', '3900', 'Earnings to date',
       coalesce(sum(CASE type WHEN 'revenue' THEN balance_cents ELSE -balance_cents END), 0)
FROM balances WHERE type IN ('revenue', 'expense')
ORDER BY code";

/// The balance sheet's totals. Assets must equal liabilities plus equity.
const BALANCE_TOTALS: &str = "
SELECT coalesce(sum(balance_cents) FILTER (WHERE type = 'asset'), 0) AS assets_cents,
       coalesce(sum(balance_cents) FILTER (WHERE type = 'liability'), 0) AS liabilities_cents,
       coalesce(sum(balance_cents) FILTER (WHERE type = 'equity'), 0)
         + coalesce(sum(CASE type WHEN 'revenue' THEN balance_cents WHEN 'expense' THEN -balance_cents END), 0) AS equity_cents
FROM balances";

impl Store {
    /// `GET /days/{day}`: the Z report, from the orders, the payments and the
    /// books, and the close when the day is closed.
    ///
    /// @param day - the business day
    pub fn z_report(&self, day: &str) -> ApiResult<Json> {
        let params = [text(day)];
        Ok(json!({
            "business_day": day,
            "orders": self.object(Z_ORDERS, &params, "orders")?,
            "payments": self.object(Z_PAYMENTS, &params, "payments")?,
            "top_items": self.objects(TOP_ITEMS, &params, &[])?,
            "books": self.objects(Z_BOOKS, &params, &[])?,
            "close": self.objects("SELECT * FROM day_close WHERE business_day = ?1", &params, &[])?.into_iter().next(),
            "tip_payouts": self.objects(
                "SELECT t.staff_id, s.name, t.minutes_worked, t.tip_cents
                 FROM tip_payout t JOIN staff s ON s.id = t.staff_id WHERE t.business_day = ?1 ORDER BY t.staff_id",
                &params,
                &[],
            )?,
        }))
    }

    /// `GET /reports/sales`: net sales for each day of a range.
    ///
    /// @param from - the first day
    /// @param to - the last day
    pub fn sales_by_day(&self, from: &str, to: &str) -> ApiResult<Vec<Json>> {
        self.check_range(from, to)?;
        self.objects(SALES_BY_DAY, &[text(from), text(to)], &[])
    }

    /// `GET /reports/hourly`: orders and takings for each hour of a day.
    ///
    /// @param day - the day
    pub fn sales_by_hour(&self, day: &str) -> ApiResult<Vec<Json>> {
        self.objects(SALES_BY_HOUR, &[text(day)], &[])
    }

    /// `GET /reports/items`: what sold in a range, and what did not.
    ///
    /// `top` keeps the first few of each category. The SQL way is to filter
    /// the ranked query in an outer `SELECT ... WHERE rank_in_category <= ?`,
    /// and inillucent 1.0.30 refuses a window function inside a derived table
    /// (task-2132 item 6), so the rows are filtered here instead.
    ///
    /// @param from - the first day
    /// @param to - the last day
    /// @param top - how many of each category to keep, or all of them
    pub fn items(&self, from: &str, to: &str, top: Option<i64>) -> ApiResult<Json> {
        self.check_range(from, to)?;
        let items: Vec<Json> = self
            .objects(ITEMS, &[text(from), text(to)], &[])?
            .into_iter()
            .filter(|item| top.map(|top| item["rank_in_category"].as_i64().unwrap_or(0) <= top).unwrap_or(true))
            .collect();
        Ok(json!({ "items": items, "unsold": self.objects(UNSOLD, &[text(from), text(to)], &[])? }))
    }

    /// `GET /reports/trial-balance`: every account's balance at the end of a day.
    ///
    /// @param as_of - the last day included
    pub fn trial_balance(&self, as_of: &str) -> ApiResult<Vec<Json>> {
        self.objects(&format!("WITH {BALANCES} {TRIAL_BALANCE}"), &[text("0000-01-01"), text(as_of)], &[])
    }

    /// `GET /reports/income-statement`: revenue, costs and profit over a range.
    ///
    /// @param from - the first day
    /// @param to - the last day
    pub fn income_statement(&self, from: &str, to: &str) -> ApiResult<Json> {
        self.check_range(from, to)?;
        let params = [text(from), text(to)];
        let mut totals = self.object(&format!("WITH {BALANCES} {INCOME_TOTALS}"), &params, "totals")?;
        let revenue = totals["net_revenue_cents"].as_i64().unwrap_or(0);
        let gross = totals["gross_profit_cents"].as_i64().unwrap_or(0);
        totals["gross_margin_percent"] = json!(if revenue == 0 { None } else { Some((gross * 1000 / revenue) as f64 / 10.0) });
        Ok(json!({
            "from": from,
            "to": to,
            "lines": self.objects(&format!("WITH {BALANCES} {INCOME_LINES}"), &params, &[])?,
            "totals": totals,
        }))
    }

    /// `GET /reports/balance-sheet`: what the business owns and owes at the
    /// end of a day.
    ///
    /// @param as_of - the last day included
    pub fn balance_sheet(&self, as_of: &str) -> ApiResult<Json> {
        let params = [text("0000-01-01"), text(as_of)];
        let mut totals = self.object(&format!("WITH {BALANCES} {BALANCE_TOTALS}"), &params, "totals")?;
        let (assets, liabilities, equity) = (
            totals["assets_cents"].as_i64().unwrap_or(0),
            totals["liabilities_cents"].as_i64().unwrap_or(0),
            totals["equity_cents"].as_i64().unwrap_or(0),
        );
        totals["balances"] = json!(assets == liabilities + equity);
        Ok(json!({
            "as_of": as_of,
            "lines": self.objects(&format!("WITH {BALANCES} {BALANCE_LINES}"), &params, &[])?,
            "totals": totals,
        }))
    }

    /// Refuses a range that runs backwards or is longer than a year.
    ///
    /// @param from - the first day
    /// @param to - the last day
    fn check_range(&self, from: &str, to: &str) -> ApiResult<()> {
        let days = self.integer("SELECT CAST(julianday(?2) - julianday(?1) AS INTEGER)", &[text(from), text(to)])?;
        if !(0..=366).contains(&days) {
            return Err(ApiError::bad_request(format!("from {from} to {to} is not a range of 1 to 367 days")));
        }
        Ok(())
    }
}
