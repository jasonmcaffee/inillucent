//! Demo data: a coffee shop with a week of trading behind it.
//!
//! Everything is written through the same [`Store`] methods the HTTP routes
//! call, so the seed exercises the same SQL, the same triggers and the same
//! checks as a real day at the counter. The numbers come from a small random
//! generator seeded with the last day, so the same command gives the same
//! database every time, and the README's example answers can be reproduced.
//!
//! Each day:
//!
//! | Time | What happens |
//! |---|---|
//! | 06:00 | staff clock in; the bakery delivers, and the dairy every third day |
//! | 06:30 to 17:30 | 30 to 50 orders, busiest before 10:00, paid by card or cash, some with tips, codes or points |
//! | during the day | a few orders are cancelled before payment, and one every other day is refunded |
//! | 18:00 | pastries left over are thrown away, staff clock out, and the day is closed |
//!
//! The last day is left open, with two paid orders still in the queue.

use serde::de::DeserializeOwned;
use serde_json::{json, Value as Json};

use crate::error::{ApiError, ApiResult};
use crate::store::{Sql, Store};

/// A small, fixed random number generator (xorshift), so the seed data is the
/// same on every machine.
struct Dice(u64);

impl Dice {
    /// A number from 0 to `below - 1`.
    ///
    /// @param below - one more than the largest answer
    fn roll(&mut self, below: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % below.max(1)
    }

    /// True `percent` times in a hundred.
    ///
    /// @param percent - how often
    fn chance(&mut self, percent: u64) -> bool {
        self.roll(100) < percent
    }
}

/// Builds a request body from JSON, the way the HTTP routes do.
///
/// @param value - the body
fn body<T: DeserializeOwned>(value: Json) -> ApiResult<T> {
    serde_json::from_value(value).map_err(|error| ApiError::bad_request(format!("seed body: {error}")))
}

/// The ingredients: name, unit, reorder level.
const INGREDIENTS: [(&str, &str, i64); 13] = [
    ("Espresso beans", "g", 3000),
    ("Whole milk", "ml", 10000),
    ("Oat milk", "ml", 3000),
    ("Chocolate sauce", "ml", 600),
    ("Vanilla syrup", "ml", 500),
    ("Chai concentrate", "ml", 1500),
    ("Cold brew concentrate", "ml", 3000),
    ("Tea bags", "each", 40),
    ("Cups", "each", 200),
    ("Croissants", "each", 0),
    ("Pain au chocolat", "each", 0),
    ("Blueberry muffins", "each", 0),
    ("Banana bread slices", "each", 0),
];

/// Fills a new database and answers a summary of it.
///
/// @param store - the database; it must have no orders yet
/// @param days - how many days of trading
/// @param until - the day after the last day of trading
pub fn seed(store: &Store, days: i64, until: &str) -> ApiResult<Json> {
    if store.integer("SELECT count(*) FROM orders", &[])? > 0 {
        return Err(ApiError::conflict("the database already has orders; seed a new file"));
    }
    if !(1..=60).contains(&days) {
        return Err(ApiError::bad_request("--days must be from 1 to 60"));
    }
    let first = store.day_after(&store.day(Some(until))?, -days)?;
    let mut dice = Dice(until.bytes().fold(0x9e37_79b9_7f4a_7c15, |hash, byte| hash.rotate_left(5) ^ u64::from(byte)));
    open_the_books(store, &first)?;
    let ids = set_up_the_shop(store, &first)?;
    for offset in 0..days {
        let day = store.day_after(&first, offset)?;
        trade_one_day(store, &mut dice, &ids, &day, offset, offset == days - 1)?;
    }
    let last = store.day_after(&first, days - 1)?;
    count_the_stock(store, &last)?;
    Ok(json!({
        "from": first,
        "to": last,
        "orders": store.integer("SELECT count(*) FROM orders", &[])?,
        "paid": store.integer("SELECT count(*) FROM orders WHERE paid_at IS NOT NULL", &[])?,
        "journal_entries": store.integer("SELECT count(*) FROM journal_entry", &[])?,
        "balance_sheet": store.balance_sheet(&last)?["totals"],
        "check": store.reconcile()?["ok"],
    }))
}

/// The ids the seed hands out, in the order it creates things.
struct Ids {
    staff: Vec<i64>,
    customers: Vec<i64>,
    items: Vec<Item>,
    modifiers: Vec<i64>,
}

/// One menu item as the seed orders it.
struct Item {
    id: i64,
    sizes: &'static [&'static str],
    milky: bool,
    weight: u64,
}

/// The owner puts money in the bank and a float in the drawer.
///
/// @param store - the database
/// @param day - the first day
fn open_the_books(store: &Store, day: &str) -> ApiResult<()> {
    store.manual_entry(&body(json!({
        "memo": "owner's investment and the drawer float",
        "at": format!("{day}T05:30:00Z"),
        "lines": [
            { "account": "1020", "debit_cents": 1_500_000 },
            { "account": "1000", "debit_cents": 20_000 },
            { "account": "3000", "credit_cents": 1_520_000 }
        ]
    }))?)?;
    Ok(())
}

/// Creates the ingredients, the menu, the staff, the customers and the
/// promotions, and receives the first deliveries.
///
/// @param store - the database
/// @param day - the first day
fn set_up_the_shop(store: &Store, day: &str) -> ApiResult<Ids> {
    for (name, unit, reorder) in INGREDIENTS {
        store.create_ingredient(&body(json!({ "name": name, "unit": unit, "reorder_level": reorder }))?)?;
    }
    store.receive_purchase(&body(json!({
        "supplier": "Northside Roasters", "invoice": format!("NR-{day}"), "at": format!("{day}T05:45:00Z"),
        "lines": [{ "ingredient_id": 1, "quantity": 12000, "cost_cents": 26400 }]
    }))?)?;
    store.receive_purchase(&body(json!({
        "supplier": "Harbour Wholesale", "invoice": format!("HW-{day}"), "at": format!("{day}T05:50:00Z"),
        "lines": [
            { "ingredient_id": 4, "quantity": 3000, "cost_cents": 2700 },
            { "ingredient_id": 5, "quantity": 2000, "cost_cents": 1600 },
            { "ingredient_id": 6, "quantity": 6000, "cost_cents": 5400 },
            { "ingredient_id": 7, "quantity": 12000, "cost_cents": 6000 },
            { "ingredient_id": 8, "quantity": 300, "cost_cents": 4500 },
            { "ingredient_id": 9, "quantity": 2000, "cost_cents": 18000 }
        ]
    }))?)?;
    let items = create_menu(store)?;
    let modifiers = vec![
        store.create_modifier(&body(json!({ "name": "Oat milk", "price_cents": 70, "ingredient_id": 3, "replaces_ingredient_id": 2 }))?)?,
        store.create_modifier(&body(json!({ "name": "Extra shot", "price_cents": 90, "ingredient_id": 1, "quantity": 18 }))?)?,
        store.create_modifier(&body(json!({ "name": "Vanilla", "price_cents": 60, "ingredient_id": 5, "quantity": 20 }))?)?,
    ];
    let staff = [("Ana", "manager"), ("Ben", "barista"), ("Chloe", "barista"), ("Dev", "barista")]
        .iter()
        .map(|(name, role)| store.create_staff(&body(json!({ "name": name, "role": role }))?))
        .collect::<ApiResult<Vec<Json>>>()?;
    let customers = ["Maya", "Noor", "Ines", "Tom", "Priya", "Leo"]
        .iter()
        .map(|name| {
            let at = format!("{day}T05:00:00Z");
            store.create_customer(&body(json!({ "name": name, "email": format!("{}@example.com", name.to_lowercase()), "at": at }))?)
        })
        .collect::<ApiResult<Vec<Json>>>()?;
    for promotion in [("WELCOME10", "percent", 10, 0), ("MORNING", "amount", 100, 800)] {
        store.create_promotion(&body(json!({
            "code": promotion.0, "kind": promotion.1, "value": promotion.2, "min_subtotal_cents": promotion.3,
            "starts_on": day, "ends_on": "2099-12-31"
        }))?)?;
    }
    Ok(Ids {
        staff: staff.iter().filter_map(|row| row["id"].as_i64()).collect(),
        customers: customers.iter().filter_map(|row| row["id"].as_i64()).collect(),
        items,
        modifiers: modifiers.iter().filter_map(|row| row["id"].as_i64()).collect(),
    })
}

/// Creates the menu: nine drinks and four pastries, with their recipes.
///
/// @param store - the database
fn create_menu(store: &Store) -> ApiResult<Vec<Item>> {
    let (beans, milk, choc, chai, cold, tea, cups) = (1, 2, 4, 6, 7, 8, 9);
    let drinks: [(&str, &str, &'static [&'static str], Json, Json, bool, u64); 9] = [
        ("ESPRESSO", "Espresso", &["regular"], json!({"regular": 300}), json!({"regular": [[beans, 18], [cups, 1]]}), false, 6),
        (
            "AMERICANO",
            "Americano",
            &["small", "large"],
            json!({"small": 350, "large": 425}),
            json!({"small": [[beans, 18], [cups, 1]], "large": [[beans, 36], [cups, 1]]}),
            false,
            10,
        ),
        (
            "LATTE",
            "Latte",
            &["small", "medium", "large"],
            json!({"small": 425, "medium": 475, "large": 525}),
            json!({"small": [[beans, 18], [milk, 240], [cups, 1]], "medium": [[beans, 18], [milk, 300], [cups, 1]],
                   "large": [[beans, 36], [milk, 360], [cups, 1]]}),
            true,
            20,
        ),
        (
            "CAPPUCCINO",
            "Cappuccino",
            &["small", "large"],
            json!({"small": 400, "large": 500}),
            json!({"small": [[beans, 18], [milk, 180], [cups, 1]], "large": [[beans, 36], [milk, 280], [cups, 1]]}),
            true,
            12,
        ),
        (
            "FLAT-WHITE",
            "Flat white",
            &["regular"],
            json!({"regular": 450}),
            json!({"regular": [[beans, 36], [milk, 150], [cups, 1]]}),
            true,
            10,
        ),
        (
            "MOCHA",
            "Mocha",
            &["small", "large"],
            json!({"small": 475, "large": 575}),
            json!({"small": [[beans, 18], [milk, 220], [choc, 30], [cups, 1]], "large": [[beans, 36], [milk, 320], [choc, 45], [cups, 1]]}),
            true,
            6,
        ),
        (
            "CHAI",
            "Chai latte",
            &["small", "large"],
            json!({"small": 450, "large": 550}),
            json!({"small": [[chai, 120], [milk, 180], [cups, 1]], "large": [[chai, 180], [milk, 260], [cups, 1]]}),
            true,
            6,
        ),
        (
            "COLD-BREW",
            "Cold brew",
            &["small", "large"],
            json!({"small": 425, "large": 525}),
            json!({"small": [[cold, 250], [cups, 1]], "large": [[cold, 400], [cups, 1]]}),
            false,
            8,
        ),
        ("TEA", "Pot of tea", &["regular"], json!({"regular": 300}), json!({"regular": [[tea, 1], [cups, 1]]}), false, 4),
    ];
    let mut items = Vec::new();
    for (sku, name, sizes, prices, recipe, milky, weight) in drinks {
        let id = create_item(store, "Coffee and tea", sku, name, true, prices, recipe)?;
        items.push(Item { id, sizes, milky, weight });
    }
    let pastries = [
        ("CROISSANT", "Croissant", 375, 10, 9),
        ("PAIN-CHOC", "Pain au chocolat", 425, 11, 6),
        ("MUFFIN", "Blueberry muffin", 395, 12, 6),
        ("BANANA-BREAD", "Banana bread", 425, 13, 5),
    ];
    for (sku, name, price, ingredient, weight) in pastries {
        let id = create_item(store, "Bakery", sku, name, false, json!({"regular": price}), json!({"regular": [[ingredient, 1]]}))?;
        items.push(Item { id, sizes: &["regular"], milky: false, weight });
    }
    Ok(items)
}

/// Creates one menu item from compact recipes: `{"small": [[ingredient, quantity], ...]}`.
///
/// @param store - the database
/// @param category - the category's name
/// @param sku - the item's code
/// @param name - the item's name
/// @param taxable - whether sales tax applies
/// @param prices - prices by size
/// @param recipe - `[ingredient, quantity]` pairs by size
fn create_item(store: &Store, category: &str, sku: &str, name: &str, taxable: bool, prices: Json, recipe: Json) -> ApiResult<i64> {
    let mut full = serde_json::Map::new();
    for (size, pairs) in recipe.as_object().into_iter().flatten() {
        let lines: Vec<Json> =
            pairs.as_array().into_iter().flatten().map(|pair| json!({ "ingredient_id": pair[0], "quantity": pair[1] })).collect();
        full.insert(size.clone(), Json::from(lines));
    }
    let item = store.create_item(&body(json!({
        "category": category, "sku": sku, "name": name, "taxable": taxable, "prices": prices, "recipe": full
    }))?)?;
    item["id"].as_i64().ok_or_else(|| ApiError::internal("the item has no id"))
}

/// One day of trading, from the morning deliveries to the close.
///
/// @param store - the database
/// @param dice - the random numbers
/// @param ids - the shop's ids
/// @param day - the day
/// @param offset - which day of the range, from 0
/// @param last - whether this is the last day, which is left open
fn trade_one_day(store: &Store, dice: &mut Dice, ids: &Ids, day: &str, offset: i64, last: bool) -> ApiResult<()> {
    let at = |time: &str| format!("{day}T{time}:00Z");
    let weekend = store.integer("SELECT strftime('%w', ?1) IN ('0', '6')", &[inillucent::Value::Text(day.to_string())])? == 1;
    let bakery = json!([
        { "ingredient_id": 10, "quantity": 24, "cost_cents": 2760 },
        { "ingredient_id": 11, "quantity": 16, "cost_cents": 2240 },
        { "ingredient_id": 12, "quantity": 16, "cost_cents": 1920 },
        { "ingredient_id": 13, "quantity": 12, "cost_cents": 1440 }
    ]);
    store.receive_purchase(&body(
        json!({ "supplier": "Crumb Bakery", "invoice": format!("CB-{day}"), "at": at("06:00"), "lines": bakery }),
    )?)?;
    if offset % 3 == 0 {
        let dairy = json!([{ "ingredient_id": 2, "quantity": 60000, "cost_cents": 6600 }, { "ingredient_id": 3, "quantity": 18000, "cost_cents": 5940 }]);
        store.receive_purchase(&body(
            json!({ "supplier": "Valley Dairy", "invoice": format!("VD-{day}"), "at": at("06:05"), "lines": dairy }),
        )?)?;
    }
    let shifts: Vec<(i64, &str, &str)> = if weekend {
        vec![(ids.staff[0], "06:00", "14:00"), (ids.staff[3], "07:00", "15:30"), (ids.staff[2], "11:00", "18:15")]
    } else {
        vec![(ids.staff[0], "06:00", "14:00"), (ids.staff[1], "06:30", "13:00"), (ids.staff[2], "11:00", "18:15")]
    };
    for (staff, start, _) in &shifts {
        store.clock_in(*staff, Some(&at(start)))?;
    }
    let count = 30 + dice.roll(12) + if weekend { 8 } else { 0 };
    let mut minutes: Vec<u64> = (0..count).map(|_| order_minute(dice)).collect();
    minutes.sort_unstable();
    let mut paid = Vec::new();
    for (n, minute) in minutes.iter().enumerate() {
        let time = format!("{:02}:{:02}", 6 + minute / 60, minute % 60);
        let staff = if *minute < 300 { shifts[0].0 } else { shifts[2].0 };
        let hold = last && n + 2 >= minutes.len();
        if let Some(paid_at) = one_order(store, dice, ids, &at(&time), staff, hold)? {
            paid.push(paid_at);
        }
    }
    if offset % 2 == 1 && paid.len() > 5 {
        let (order, paid_at) = &paid[dice.roll(paid.len() as u64 - 3) as usize];
        store.refund(*order, Some(&store.minutes_after(paid_at, 50)?))?;
    }
    if last {
        return Ok(());
    }
    close_the_day(store, dice, day, &shifts)
}

/// Picks a minute after 06:30 for an order, busier in the morning.
///
/// @param dice - the random numbers
fn order_minute(dice: &mut Dice) -> u64 {
    match dice.roll(10) {
        0..=4 => 30 + dice.roll(210),
        5..=7 => 240 + dice.roll(240),
        _ => 480 + dice.roll(210),
    }
}

/// Takes one order: opens it, maybe applies a code or points, pays it and
/// hands it over. A few are cancelled instead, and `hold` leaves it paid in
/// the queue.
///
/// @param store - the database
/// @param dice - the random numbers
/// @param ids - the shop's ids
/// @param at - when the order is opened
/// @param staff - who takes it
/// @param hold - leave it paid and not handed over
fn one_order(store: &Store, dice: &mut Dice, ids: &Ids, at: &str, staff: i64, hold: bool) -> ApiResult<Option<(i64, String)>> {
    let lines: Vec<Json> = (0..1 + dice.roll(3).min(dice.roll(3))).map(|_| one_line(dice, ids)).collect();
    let customer = if dice.chance(40) { Some(ids.customers[dice.roll(ids.customers.len() as u64) as usize]) } else { None };
    let order = store.create_order(&body(json!({ "customer_id": customer, "staff_id": staff, "at": at, "lines": lines,
        "channel": if dice.chance(15) { "mobile" } else { "counter" } }))?)?;
    let id = order["id"].as_i64().ok_or_else(|| ApiError::internal("the order has no id"))?;
    if dice.chance(4) {
        store.cancel(id, Some(at))?;
        return Ok(None);
    }
    if customer.is_some() && order["points"]["balance"].as_i64().unwrap_or(0) >= 100 && dice.chance(50) {
        store.redeem(id, 100)?;
    } else if dice.chance(6) {
        store.apply_promotion(id, "welcome10")?;
    } else if order["subtotal_cents"].as_i64().unwrap_or(0) >= 800 && &at[11..13] < "10" && dice.chance(30) {
        store.apply_promotion(id, "MORNING")?;
    }
    let total = store.receipt(id)?["total_cents"].as_i64().unwrap_or(0);
    let paid_at = store.minutes_after(at, 1 + dice.roll(2) as i64)?;
    let payments = if total == 0 { json!([]) } else { json!([tender(dice, total)]) };
    store.pay(id, &body(json!({ "at": paid_at, "payments": payments }))?)?;
    if !hold {
        store.fulfil(id, Some(&store.minutes_after(&paid_at, 2 + dice.roll(5) as i64)?))?;
    }
    Ok(Some((id, paid_at)))
}

/// One line: an item by its popularity, a size, and sometimes a modifier.
///
/// @param dice - the random numbers
/// @param ids - the shop's ids
fn one_line(dice: &mut Dice, ids: &Ids) -> Json {
    let total: u64 = ids.items.iter().map(|item| item.weight).sum();
    let mut pick = dice.roll(total);
    let item = ids.items.iter().find(|item| {
        let hit = pick < item.weight;
        pick = pick.saturating_sub(item.weight);
        hit
    });
    let item = item.unwrap_or(&ids.items[0]);
    let size = item.sizes[dice.roll(item.sizes.len() as u64) as usize];
    let mut modifiers = Vec::new();
    if item.milky && dice.chance(25) {
        modifiers.push(ids.modifiers[0]);
    }
    if item.sizes.len() > 1 && dice.chance(12) {
        modifiers.push(ids.modifiers[1]);
    }
    if item.milky && dice.chance(10) {
        modifiers.push(ids.modifiers[2]);
    }
    json!({ "menu_item_id": item.id, "size": size, "quantity": if dice.chance(10) { 2 } else { 1 }, "modifiers": modifiers })
}

/// A payment for the whole total: card most of the time, cash otherwise,
/// with a tip now and then and cash handed over in round notes.
///
/// @param dice - the random numbers
/// @param total - the order total
fn tender(dice: &mut Dice, total: i64) -> Json {
    if dice.chance(62) {
        let tip = if dice.chance(45) { [50, 75, 100, 150][dice.roll(4) as usize] } else { 0 };
        json!({ "method": "card", "tip_cents": tip })
    } else {
        let tip = if dice.chance(15) { 50 } else { 0 };
        let note = [500, 1000, 2000][dice.roll(3) as usize];
        let tendered = ((total + tip + note - 1) / note) * note;
        json!({ "method": "cash", "tip_cents": tip, "tendered_cents": tendered })
    }
}

/// The end of a day: the unsold pastries are thrown out, the staff clock out,
/// an invoice three days old is paid, and the day is closed with a cash
/// count that is right most days and a little out on some.
///
/// @param store - the database
/// @param dice - the random numbers
/// @param day - the day
/// @param shifts - who worked, and when they left
fn close_the_day(store: &Store, dice: &mut Dice, day: &str, shifts: &[(i64, &str, &str)]) -> ApiResult<()> {
    let at = |time: &str| format!("{day}T{time}:00Z");
    for ingredient in 10..=13 {
        let left = store.integer("SELECT on_hand FROM ingredient WHERE id = ?1", &[inillucent::Value::Integer(ingredient)])?;
        if left > 0 {
            store.waste(&body(json!({ "ingredient_id": ingredient, "quantity": left, "note": "unsold at close", "at": at("18:00") }))?)?;
        }
    }
    for (staff, _, end) in shifts {
        store.clock_out(*staff, Some(&at(end)))?;
    }
    let old = store.integer(
        "SELECT coalesce(min(id), 0) FROM purchase WHERE paid_at IS NULL AND date(received_at) <= date(?1, '-3 days')",
        &[inillucent::Value::Text(day.to_string())],
    )?;
    if old > 0 {
        store.pay_supplier(old, Some(&at("17:00")))?;
    }
    let drawer = store.account_ledger("1000", day, day)?["closing_balance_cents"].as_i64().unwrap_or(0);
    let tips: i64 = store.tip_pool(day)?.iter().filter_map(|share| share["tip_cents"].as_i64()).sum();
    let error = match dice.roll(10) {
        0..=6 => 0,
        7 | 8 => -(dice.roll(300) as i64),
        _ => dice.roll(200) as i64,
    };
    store.close_day(day, &body(json!({ "counted_cash_cents": drawer - tips.max(0) + error, "at": at("18:30") }))?)?;
    Ok(())
}

/// A stock count on the last morning, which finds a little less coffee and
/// milk than the books expect.
///
/// @param store - the database
/// @param day - the last day
fn count_the_stock(store: &Store, day: &str) -> ApiResult<()> {
    let beans = store.integer("SELECT on_hand FROM ingredient WHERE id = 1", &[])?;
    let milk = store.integer("SELECT on_hand FROM ingredient WHERE id = 2", &[])?;
    store.count(&body(json!({
        "at": format!("{day}T18:45:00Z"),
        "counts": [{ "ingredient_id": 1, "counted": beans - 140 }, { "ingredient_id": 2, "counted": milk - 900 }]
    }))?)?;
    Ok(())
}
