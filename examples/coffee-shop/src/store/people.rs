//! People: loyalty customers and their points, and staff and their shifts.
//!
//! A customer's points are never stored as a number that code adds to. They
//! are rows in `loyalty_entry`, written by the `order_paid` and
//! `order_refunded` triggers, and the balance is their sum. So the balance
//! always matches its history, and the history says why it is what it is.

use serde::Deserialize;
use serde_json::Value as Json;

use super::{int, opt_text, text, Sql, Store};
use crate::error::{ApiError, ApiResult};

/// The body of `POST /customers`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewCustomer {
    /// Their name.
    pub name: String,
    /// Their email, unique ignoring case.
    #[serde(default)]
    pub email: Option<String>,
    /// When they joined. Now when left out.
    #[serde(default)]
    pub at: Option<String>,
}

/// The body of `POST /staff`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewStaff {
    /// Their name.
    pub name: String,
    /// `barista` or `manager`.
    #[serde(default)]
    pub role: Option<String>,
}

/// Every customer with their visits, spend and points, ranked by spend.
///
/// The spend comes from a derived table grouped by customer, joined once,
/// so each customer row is not multiplied by their orders and then by their
/// points. `ntile(4)` over the spend puts each customer in a quarter: 1 is the
/// quarter that spends the most.
const CUSTOMERS: &str = "
SELECT c.id, c.name, c.email, c.created_at,
       coalesce(o.visits, 0) AS visits,
       coalesce(o.spent_cents, 0) AS spent_cents,
       o.last_visit,
       p.points,
       rank() OVER (ORDER BY coalesce(o.spent_cents, 0) DESC) AS spend_rank,
       ntile(4) OVER (ORDER BY coalesce(o.spent_cents, 0) DESC, c.id) AS spend_quarter
FROM customer c
JOIN customer_points p ON p.customer_id = c.id
LEFT JOIN (SELECT customer_id, count(*) AS visits, sum(total_cents) AS spent_cents, max(paid_at) AS last_visit
           FROM orders WHERE status IN ('paid', 'fulfilled') GROUP BY customer_id) o ON o.customer_id = c.id
ORDER BY spend_rank, c.id";

/// A customer's favourite item: the one they have bought most of, by a
/// correlated subquery that sorts their items and keeps the first.
const FAVOURITE: &str = "
SELECT m.name, sum(l.quantity) AS bought
FROM order_line l
JOIN orders o ON o.id = l.order_id
JOIN menu_item m ON m.id = l.menu_item_id
WHERE o.customer_id = ?1 AND o.status IN ('paid', 'fulfilled')
GROUP BY m.id
ORDER BY bought DESC, m.name
LIMIT 1";

/// A customer's points history with the balance after each entry, from a
/// running `sum()` over the entries in order.
const POINTS_HISTORY: &str = "
SELECT le.id, le.at, le.reason, le.points, o.ticket, o.business_day,
       sum(le.points) OVER (ORDER BY le.id) AS balance
FROM loyalty_entry le
LEFT JOIN orders o ON o.id = le.order_id
WHERE le.customer_id = ?1
ORDER BY le.id";

impl Store {
    /// `POST /customers`: a new loyalty customer, who starts with 50 points.
    ///
    /// @param customer - the new customer
    pub fn create_customer(&self, customer: &NewCustomer) -> ApiResult<Json> {
        let at = self.timestamp(customer.at.as_deref())?;
        let tx = self.begin()?;
        let id = tx.integer(
            "INSERT INTO customer (name, email, created_at) VALUES (trim(?1), lower(trim(?2)), ?3) RETURNING id",
            &[text(&customer.name), opt_text(customer.email.as_deref()), text(&at)],
        )?;
        tx.run("INSERT INTO loyalty_entry (customer_id, points, reason, at) VALUES (?1, 50, 'welcome', ?2)", &[int(id), text(&at)])?;
        tx.commit()?;
        self.customer(id)
    }

    /// `GET /customers`: every customer, ranked by what they spend.
    pub fn customers(&self) -> ApiResult<Vec<Json>> {
        self.objects(CUSTOMERS, &[], &[])
    }

    /// `GET /customers/{id}`: a customer, their favourite, their points history
    /// and their last orders.
    ///
    /// Their rank comes from the ranked list of every customer, picked out in
    /// Rust. Filtering the ranked query in an outer `SELECT` is the SQL way,
    /// and inillucent 1.0.30 refuses a window function inside a derived table
    /// (task-2132 item 6).
    ///
    /// @param id - the customer
    pub fn customer(&self, id: i64) -> ApiResult<Json> {
        let mut customer = self
            .customers()?
            .into_iter()
            .find(|row| row["id"].as_i64() == Some(id))
            .ok_or_else(|| ApiError::not_found(format!("customer {id}")))?;
        customer["favourite"] = self.objects(FAVOURITE, &[int(id)], &[])?.into_iter().next().unwrap_or(Json::Null);
        customer["points_history"] = Json::from(self.objects(POINTS_HISTORY, &[int(id)], &[])?);
        customer["recent_orders"] = Json::from(self.objects(
            "SELECT id, business_day, ticket, status, total_cents, paid_at FROM orders
             WHERE customer_id = ?1 ORDER BY opened_at DESC LIMIT 5",
            &[int(id)],
            &[],
        )?);
        Ok(customer)
    }

    /// `POST /staff`.
    ///
    /// @param staff - the new person
    pub fn create_staff(&self, staff: &NewStaff) -> ApiResult<Json> {
        let id = self.integer(
            "INSERT INTO staff (name, role) VALUES (trim(?1), coalesce(?2, 'barista')) RETURNING id",
            &[text(&staff.name), opt_text(staff.role.as_deref())],
        )?;
        self.object("SELECT id, name, role FROM staff WHERE id = ?1", &[int(id)], "staff")
    }

    /// `GET /staff`: everyone, whether they are clocked in, and the minutes
    /// they worked on a day.
    ///
    /// @param day - the day to count minutes on
    pub fn staff(&self, day: &str) -> ApiResult<Vec<Json>> {
        self.objects(
            "SELECT s.id, s.name, s.role,
                    max(sh.ended_at IS NULL AND sh.id IS NOT NULL) AS clocked_in,
                    coalesce(sum((unixepoch(sh.ended_at) - unixepoch(sh.started_at)) / 60)
                             FILTER (WHERE date(sh.started_at) = ?1), 0) AS minutes_on_day
             FROM staff s LEFT JOIN shift sh ON sh.staff_id = s.id
             GROUP BY s.id ORDER BY s.name",
            &[text(day)],
            &[],
        )
    }

    /// `POST /staff/{id}/clock-in`. The partial unique index `shift_one_open`
    /// refuses a second open shift, and that becomes `409`.
    ///
    /// @param id - the person
    /// @param at - when, or now
    pub fn clock_in(&self, id: i64, at: Option<&str>) -> ApiResult<Json> {
        let at = self.timestamp(at)?;
        let shift = self.integer("INSERT INTO shift (staff_id, started_at) VALUES (?1, ?2) RETURNING id", &[int(id), text(&at)])?;
        self.object("SELECT * FROM shift WHERE id = ?1", &[int(shift)], "shift")
    }

    /// `POST /staff/{id}/clock-out`: ends the open shift.
    ///
    /// @param id - the person
    /// @param at - when, or now
    pub fn clock_out(&self, id: i64, at: Option<&str>) -> ApiResult<Json> {
        let at = self.timestamp(at)?;
        let rows = self.objects(
            "UPDATE shift SET ended_at = ?2 WHERE staff_id = ?1 AND ended_at IS NULL
             RETURNING id, staff_id, started_at, ended_at, (unixepoch(ended_at) - unixepoch(started_at)) / 60 AS minutes",
            &[int(id), text(&at)],
            &[],
        )?;
        rows.into_iter().next().ok_or_else(|| ApiError::conflict(format!("staff {id} is not clocked in")))
    }
}
