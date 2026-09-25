//! The menu: categories, items, a price for each size, what each size is made
//! of, the modifiers a drink can take, and what each item earns.
//!
//! A new item arrives as one JSON body with its prices and recipes nested in
//! it. The store binds those nested parts as JSON text and lets `json_each`
//! turn them into rows inside an `INSERT ... SELECT`, so an item with three
//! sizes and twelve recipe lines is three statements, not sixteen.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Value as Json};

use super::{int, json_text, text, Record, Sql, Store};
use crate::error::{ApiError, ApiResult};

/// The body of `POST /menu/items`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewItem {
    /// The category's name. A category that does not exist yet is created.
    pub category: String,
    /// A short code in capitals, such as `LATTE`.
    pub sku: String,
    /// The name on the menu board.
    pub name: String,
    /// Whether sales tax applies. Most food to take away is not taxed here.
    #[serde(default = "yes")]
    pub taxable: bool,
    /// The price in cents of each size it is sold in, such as `{"small": 425}`.
    pub prices: BTreeMap<String, i64>,
    /// What each size is made of, by size.
    #[serde(default)]
    pub recipe: BTreeMap<String, Vec<RecipeLine>>,
}

/// One ingredient of one size of an item.
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeLine {
    /// The ingredient.
    pub ingredient_id: i64,
    /// How much of it, in the ingredient's unit.
    pub quantity: i64,
}

/// The body of `PUT /menu/items/{id}/prices`: new prices by size. A size not
/// named keeps its price, and a size the item did not have is added.
pub type PriceChange = BTreeMap<String, i64>;

/// The body of `POST /menu/modifiers`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewModifier {
    /// Its name, such as `Oat milk`.
    pub name: String,
    /// What it adds to the price, in cents.
    #[serde(default)]
    pub price_cents: i64,
    /// The ingredient it adds or swaps in.
    #[serde(default)]
    pub ingredient_id: Option<i64>,
    /// How much of that ingredient it adds, when it adds one.
    #[serde(default)]
    pub quantity: i64,
    /// The ingredient it replaces, when it replaces one.
    #[serde(default)]
    pub replaces_ingredient_id: Option<i64>,
}

/// The body of `POST /promotions`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewPromotion {
    /// The code a customer gives, matched ignoring case.
    pub code: String,
    /// `percent` or `amount`.
    pub kind: String,
    /// The percent off, or the cents off.
    pub value: i64,
    /// The smallest subtotal it applies to, in cents.
    #[serde(default)]
    pub min_subtotal_cents: i64,
    /// The first day it applies.
    pub starts_on: String,
    /// The last day it applies.
    pub ends_on: String,
}

/// serde's default for `taxable`.
fn yes() -> bool {
    true
}

/// Every active item with its prices, grouped under its category.
///
/// `json_group_object(size, price_cents)` folds the price rows of one item
/// into `{"small": 425, "large": 525}` in the query itself. The categories
/// are grouped in Rust: nesting the items with
/// `json_group_array(json_object(...))` gives an array of strings in
/// inillucent 1.0.30 (task-2132 item 4).
const MENU: &str = "
SELECT c.name AS category, m.id, m.sku, m.name, m.taxable, m.active,
       json_group_object(p.size, p.price_cents ORDER BY p.price_cents) AS prices
FROM category c
JOIN menu_item m ON m.category_id = c.id
JOIN menu_price p ON p.menu_item_id = m.id
WHERE m.active = 1 OR ?1
GROUP BY m.id
ORDER BY c.position, c.name, m.name";

/// Each size of each item with its price, what its recipe costs at today's
/// average ingredient costs, and its rank by margin across the whole menu and
/// inside its category.
const MARGINS: &str = "
SELECT menu_item_id, sku, name, category, size, price_cents, cost_cents,
       price_cents - cost_cents AS margin_cents,
       round(100.0 * (price_cents - cost_cents) / nullif(price_cents, 0), 1) AS margin_percent,
       rank() OVER (ORDER BY (price_cents - cost_cents) * 1.0 / nullif(price_cents, 0) DESC) AS rank_on_menu,
       rank() OVER (PARTITION BY category ORDER BY price_cents - cost_cents DESC) AS rank_in_category
FROM menu_margin
ORDER BY rank_on_menu, sku, price_cents";

impl Store {
    /// `GET /menu`: every item under its category, and every modifier.
    ///
    /// @param all - include items taken off the menu
    pub fn menu(&self, all: bool) -> ApiResult<Json> {
        let items = self.objects(MENU, &[int(all as i64)], &["prices"])?;
        let mut categories: Vec<Json> = Vec::new();
        for item in items {
            let name = item["category"].clone();
            if categories.last().map(|category| category["name"] != name).unwrap_or(true) {
                categories.push(json!({ "name": name, "items": [] }));
            }
            if let Some(list) = categories.last_mut().and_then(|category| category["items"].as_array_mut()) {
                list.push(item);
            }
        }
        Ok(json!({ "categories": categories, "modifiers": self.modifiers()? }))
    }

    /// Every modifier, with the ingredient it adds or replaces.
    pub fn modifiers(&self) -> ApiResult<Vec<Json>> {
        self.objects(
            "SELECT m.id, m.name, m.price_cents, i.name AS ingredient, m.quantity, r.name AS replaces
             FROM modifier m
             LEFT JOIN ingredient i ON i.id = m.ingredient_id
             LEFT JOIN ingredient r ON r.id = m.replaces_ingredient_id
             ORDER BY m.name",
            &[],
            &[],
        )
    }

    /// `POST /menu/modifiers`.
    ///
    /// @param modifier - the new modifier
    pub fn create_modifier(&self, modifier: &NewModifier) -> ApiResult<Json> {
        let id = self.integer(
            "INSERT INTO modifier (name, price_cents, ingredient_id, quantity, replaces_ingredient_id)
             VALUES (trim(?1), ?2, ?3, ?4, ?5) RETURNING id",
            &[
                text(&modifier.name),
                int(modifier.price_cents),
                super::opt_int(modifier.ingredient_id),
                int(modifier.quantity),
                super::opt_int(modifier.replaces_ingredient_id),
            ],
        )?;
        self.object("SELECT id, name, price_cents FROM modifier WHERE id = ?1", &[int(id)], "modifier")
    }

    /// `POST /menu/items`: an item with its prices and recipes, in one transaction.
    ///
    /// The category is created when it is new. `ON CONFLICT DO NOTHING` then a
    /// `SELECT` is used, and not `RETURNING`, because a row that already existed
    /// is not returned by `RETURNING` when nothing was inserted.
    ///
    /// @param item - the new item
    pub fn create_item(&self, item: &NewItem) -> ApiResult<Json> {
        if item.prices.is_empty() {
            return Err(ApiError::bad_request("an item needs a price for at least one size"));
        }
        let tx = self.begin()?;
        tx.run("INSERT INTO category (name) VALUES (trim(?1)) ON CONFLICT (name) DO NOTHING", &[text(&item.category)])?;
        let id = tx.integer(
            "INSERT INTO menu_item (category_id, sku, name, taxable)
             SELECT id, upper(trim(?2)), trim(?3), ?4 FROM category WHERE name = trim(?1)
             RETURNING id",
            &[text(&item.category), text(&item.sku), text(&item.name), int(item.taxable as i64)],
        )?;
        tx.run(
            "INSERT INTO menu_price (menu_item_id, size, price_cents) SELECT ?1, key, value FROM json_each(?2)",
            &[int(id), json_text(&item.prices)?],
        )?;
        insert_recipe(&tx, id, &item.recipe)?;
        tx.commit()?;
        self.item(id)
    }

    /// `GET /menu/items/{id}`: an item, each size's price and margin, and its recipes.
    ///
    /// Each size's rank is its rank on the whole menu, so the margins of every
    /// item are ranked and this item's rows are picked out in Rust. A `WHERE`
    /// on the ranked query would rank this item's sizes among themselves, and
    /// the SQL way, an outer `SELECT` over the ranked rows, is refused by
    /// inillucent 1.0.30 (task-2132 item 6).
    ///
    /// @param id - the item
    pub fn item(&self, id: i64) -> ApiResult<Json> {
        let mut item = self.object(
            "SELECT m.id, m.sku, m.name, c.name AS category, m.taxable, m.active
             FROM menu_item m JOIN category c ON c.id = m.category_id WHERE m.id = ?1",
            &[int(id)],
            &format!("menu item {id}"),
        )?;
        let sizes: Vec<Json> = self.margins()?.into_iter().filter(|row| row["menu_item_id"].as_i64() == Some(id)).collect();
        item["sizes"] = Json::from(sizes);
        item["recipe"] = Json::from(self.objects(
            "SELECT r.size, i.id AS ingredient_id, i.name AS ingredient, r.quantity, i.unit,
                    (r.quantity * i.unit_cost_micros + 5000) / 10000 AS cost_cents
             FROM recipe r JOIN ingredient i ON i.id = r.ingredient_id
             JOIN menu_price p ON p.menu_item_id = r.menu_item_id AND p.size = r.size
             WHERE r.menu_item_id = ?1
             ORDER BY p.price_cents, i.name",
            &[int(id)],
            &[],
        )?);
        Ok(item)
    }

    /// `PUT /menu/items/{id}/prices`: sets the price of each size named.
    ///
    /// `ON CONFLICT DO UPDATE` changes a size the item has and inserts one it
    /// does not. Lines already on open orders keep the price they were added
    /// at, because `order_line` copied it.
    ///
    /// @param id - the item
    /// @param prices - new prices by size
    pub fn change_prices(&self, id: i64, prices: &PriceChange) -> ApiResult<Json> {
        self.require_item(id)?;
        self.run(
            "INSERT INTO menu_price (menu_item_id, size, price_cents)
             SELECT ?1, key, value FROM json_each(?2) WHERE true
             ON CONFLICT (menu_item_id, size) DO UPDATE SET price_cents = excluded.price_cents",
            &[int(id), json_text(prices)?],
        )?;
        self.item(id)
    }

    /// `PATCH /menu/items/{id}`: takes an item off the menu or puts it back.
    ///
    /// Items are never deleted, because past orders point at them.
    ///
    /// @param id - the item
    /// @param active - whether it is on the menu
    pub fn set_item_active(&self, id: i64, active: bool) -> ApiResult<Json> {
        self.require_item(id)?;
        self.run("UPDATE menu_item SET active = ?2 WHERE id = ?1", &[int(id), int(active as i64)])?;
        self.item(id)
    }

    /// `POST /promotions`. The schema's CHECK constraints refuse a percent
    /// over 100, an end before the start, and a day that is not a real day.
    ///
    /// @param promotion - the new promotion
    pub fn create_promotion(&self, promotion: &NewPromotion) -> ApiResult<Json> {
        let id = self.integer(
            "INSERT INTO promotion (code, kind, value, min_subtotal_cents, starts_on, ends_on)
             VALUES (upper(trim(?1)), ?2, ?3, ?4, ?5, ?6) RETURNING id",
            &[
                text(&promotion.code),
                text(&promotion.kind),
                int(promotion.value),
                int(promotion.min_subtotal_cents),
                text(&promotion.starts_on),
                text(&promotion.ends_on),
            ],
        )?;
        self.object("SELECT * FROM promotion WHERE id = ?1", &[int(id)], "promotion")
    }

    /// `GET /promotions`: every promotion, with how many paid orders used it
    /// and what it gave away.
    pub fn promotions(&self) -> ApiResult<Vec<Json>> {
        self.objects(
            "SELECT p.*, count(o.id) AS orders, coalesce(sum(o.discount_cents), 0) AS discount_given_cents
             FROM promotion p LEFT JOIN orders o ON o.promotion_id = p.id AND o.paid_at IS NOT NULL
             GROUP BY p.id ORDER BY p.starts_on, p.code",
            &[],
            &[],
        )
    }

    /// `GET /reports/margins`: every size of every item, ranked by margin.
    pub fn margins(&self) -> ApiResult<Vec<Json>> {
        self.objects(MARGINS, &[], &[])
    }

    /// Answers `404` when an item does not exist.
    ///
    /// @param id - the item
    fn require_item(&self, id: i64) -> ApiResult<()> {
        match self.integer("SELECT count(*) FROM menu_item WHERE id = ?1", &[int(id)])? {
            0 => Err(ApiError::not_found(format!("menu item {id}"))),
            _ => Ok(()),
        }
    }
}

/// Inserts an item's recipes, one statement per size.
///
/// The recipes arrive as `{"small": [{"ingredient_id": 1, "quantity": 18}], ...}`.
/// The SQLite way is one statement for every size: an outer `json_each` walks
/// the sizes, and an inner `json_each(s.value)` walks each size's
/// ingredients. inillucent 1.0.30 refuses a `json_each` whose argument comes
/// from another `json_each` (task-2134 item 6), so the sizes are walked here
/// and each size's list is bound on its own. The composite foreign key on
/// `recipe` refuses a size the item has no price for.
///
/// @param sql - the open transaction
/// @param id - the item
/// @param recipe - the recipes by size
fn insert_recipe(sql: &impl Sql, id: i64, recipe: &BTreeMap<String, Vec<RecipeLine>>) -> ApiResult<()> {
    for (size, lines) in recipe {
        sql.run(
            "INSERT INTO recipe (menu_item_id, size, ingredient_id, quantity)
             SELECT ?1, ?2, value ->> '$.ingredient_id', value ->> '$.quantity' FROM json_each(?3)",
            &[int(id), text(size), json_text(lines)?],
        )?;
    }
    Ok(())
}

/// Reads an item's current price for one size, and whether it is taxed.
///
/// @param sql - the open transaction
/// @param item - the item
/// @param size - the size
pub fn price_of(sql: &impl Sql, item: i64, size: &str) -> ApiResult<(i64, bool)> {
    let rows = sql.rows(
        "SELECT p.price_cents, m.taxable, m.active FROM menu_price p JOIN menu_item m ON m.id = p.menu_item_id
         WHERE p.menu_item_id = ?1 AND p.size = ?2",
        &[int(item), text(size)],
    )?;
    match Record::first(&rows) {
        Some(row) if row.int("active") == 1 => Ok((row.int("price_cents"), row.int("taxable") == 1)),
        Some(_) => Err(ApiError::conflict(format!("menu item {item} is not on the menu"))),
        None => Err(ApiError::not_found(format!("menu item {item} in size {size}"))),
    }
}
