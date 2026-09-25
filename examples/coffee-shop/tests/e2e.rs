//! End to end tests: the real server, the real database file, over HTTP.
//!
//! Each test starts `coffee-server serve` on a new database in a folder of its
//! own, on a free port, and calls it the way a till or a back office screen
//! would. Every assertion checks a value the service returned. The numbers
//! are worked out by hand from the prices and costs `support::shop` sets up,
//! and each test says how, so a changed number points at the rule that moved.
//!
//! Every event is dated on `support::DAY` with an explicit time, so the tests
//! give the same answer on any day they run.

mod support;

use serde_json::{json, Value};
use support::{at, entries, finish, id, int, lines, order, pay_by_card, scratch, shop, Server, DAY, PROGRAM};

/// Answers `(account, debit, credit)` rows the way the tests write them.
///
/// @param rows - `(account, debit, credit)`
fn expected(rows: &[(&str, i64, i64)]) -> Vec<(String, i64, i64)> {
    rows.iter().map(|(account, debit, credit)| (account.to_string(), *debit, *credit)).collect()
}

/// Two large oat lattes with an extra shot, merged into one line, and a
/// croissant; paid half in cash and half by card with a tip. The sale posts
/// one balanced journal entry, uses the right stock, and earns points.
#[test]
fn an_order_is_priced_paid_and_posted_to_the_books() {
    let folder = scratch("sale");
    let server = Server::start(&folder.join("coffee.rdb"));
    let shop = shop(&server);

    let latte = json!({ "menu_item_id": shop.latte, "size": "large", "modifiers": [shop.shot, shop.oat] });
    let same_latte = json!({ "menu_item_id": shop.latte, "size": "large", "modifiers": [shop.oat, shop.shot, shop.oat] });
    let receipt = order(
        &server,
        json!({ "customer_id": shop.maya, "staff_id": shop.ben, "at": at("08:00"),
                "lines": [latte, same_latte, { "menu_item_id": shop.croissant }] }),
    );
    let order_id = id(&receipt);
    let lines_on_receipt = receipt["lines"].as_array().expect("lines");
    assert_eq!(lines_on_receipt.len(), 2, "the same drink with the same modifiers in any order is one line: {receipt:#}");
    assert_eq!(lines_on_receipt[0]["quantity"], json!(2));
    assert_eq!(lines_on_receipt[0]["unit_price_cents"], json!(525 + 70 + 90), "the size's price plus each modifier's");
    assert_eq!(lines_on_receipt[0]["modifiers"], json!("Extra shot, Oat milk"));
    assert_eq!(receipt["ticket"], json!(1));
    // 2 x 685 + 375 = 1745. Only the lattes are taxed: 1370 x 8.25% = 113.03, so 113.
    assert_eq!((int(&receipt, "subtotal_cents"), int(&receipt, "tax_cents"), int(&receipt, "total_cents")), (1745, 113, 1858));

    let paid = server.post(
        &format!("/orders/{order_id}/pay"),
        json!({ "at": at("08:02"), "payments": [
            { "method": "cash", "amount_cents": 1000, "tendered_cents": 2000 },
            { "method": "card", "tip_cents": 150 }
        ]}),
    );
    assert_eq!(paid["status"], json!("paid"));
    let payments = paid["payments"].as_array().expect("payments");
    assert_eq!((int(&payments[0], "change_cents"), int(&payments[1], "amount_cents")), (1000, 858), "the card pays what the cash left");
    assert_eq!(
        paid["points"],
        json!({ "earned": 17, "redeemed": 0, "reversed": 0, "balance": 67 }),
        "a point a dollar, on top of 50 welcome points"
    );

    // Stock used, valued at average cost and rounded to the cent: beans (36 + 18) x 2 = 108 g
    // at 2.2 cents = 237.6, so 238; oat milk 360 x 2 = 720 ml at 0.33 = 237.6, so 238; two cups
    // at 9 cents; one croissant at 115. No whole milk: the oat milk replaced it.
    let sale = &entries(&server, DAY, "sale")[0];
    assert_eq!(
        lines(sale),
        expected(&[
            ("1000", 1000, 0),
            ("1010", 1008, 0),
            ("4000", 0, 1745),
            ("2000", 0, 113),
            ("2100", 0, 150),
            ("5000", 609, 0),
            ("1200", 0, 609)
        ])
    );
    let used: Vec<(i64, i64)> = [1, 2, 3, 4, 5]
        .iter()
        .map(|ingredient| last_movement(&server, *ingredient))
        .map(|m| (int(&m, "change"), int(&m, "cost_cents")))
        .collect();
    assert_eq!(
        used,
        [(-108, -238), (10000, 1100), (-720, -238), (-2, -18), (-1, -115)],
        "whole milk's last movement is still its purchase"
    );

    let check = server.get("/ledger/check");
    assert_eq!(check["ok"], json!(true), "{check:#}");
    assert_eq!(check["inventory"]["ledger_cents"], json!(7000 - 609), "the inventory account equals the stock movements");
    finish(server, &folder);
}

/// Answers an ingredient's last stock movement.
///
/// @param server - the running server
/// @param ingredient - the ingredient
fn last_movement(server: &Server, ingredient: i64) -> Value {
    server.get(&format!("/inventory/{ingredient}/movements")).as_array().and_then(|all| all.last().cloned()).expect("a movement")
}

/// What the triggers refuse, what a price change reaches, and a refund.
#[test]
fn the_order_lifecycle_is_enforced_by_the_schema() {
    let folder = scratch("lifecycle");
    let server = Server::start(&folder.join("coffee.rdb"));
    let shop = shop(&server);
    let small = json!({ "menu_item_id": shop.latte, "size": "small" });

    let first = id(&order(&server, json!({ "at": at("09:00"), "lines": [small] })));
    let message = server.fails(409, "POST", &format!("/orders/{first}/fulfil?at={}", at("09:01")), None);
    assert!(message.contains("lifecycle"), "{message}");
    assert!(server.fails(409, "POST", &format!("/orders/{first}/refund"), None).contains("only a paid order"));

    let short = json!({ "at": at("09:01"), "payments": [{ "method": "card", "amount_cents": 100 }] });
    assert!(server.fails(409, "POST", &format!("/orders/{first}/pay"), Some(short)).contains("add up to 100 cents"));
    let still_open = server.get(&format!("/orders/{first}"));
    assert_eq!(
        (still_open["status"].clone(), still_open["payments"].clone()),
        (json!("open"), json!([])),
        "the refused payment was rolled back"
    );

    let empty = id(&order(&server, json!({ "at": at("09:02") })));
    assert!(server
        .fails(409, "POST", &format!("/orders/{empty}/pay"), Some(json!({ "payments": [{ "method": "card" }] })))
        .contains("no lines"));
    assert_eq!(server.act(&format!("/orders/{empty}/cancel?at={}", at("09:03")))["status"], json!("cancelled"));
    assert!(server
        .fails(409, "POST", &format!("/orders/{empty}/lines"), Some(small.clone()))
        .contains("lines can only be added to an open order"));

    server.ok("PUT", &format!("/menu/items/{}/prices", shop.latte), Some(json!({ "small": 450 })));
    assert_eq!(server.get(&format!("/orders/{first}"))["lines"][0]["unit_price_cents"], json!(425), "an open order keeps its price");
    let later = order(&server, json!({ "at": at("09:04"), "lines": [small] }));
    assert_eq!(later["lines"][0]["unit_price_cents"], json!(450));
    assert_eq!(later["ticket"], json!(3), "tickets count up through the day");

    // 425 + 8.25% tax (35.06, so 35) = 460.
    let paid = pay_by_card(&server, first, "09:05", 0);
    assert_eq!(int(&paid, "total_cents"), 460);
    let line = paid["lines"][0]["id"].as_i64().expect("a line id");
    assert!(server
        .fails(409, "POST", &format!("/orders/{first}/lines"), Some(small.clone()))
        .contains("lines can only be added to an open order"));
    let change = Some(json!({ "quantity": 3 }));
    assert!(server
        .fails(409, "PATCH", &format!("/orders/{first}/lines/{line}"), change)
        .contains("lines can only be changed on an open order"));
    assert!(server.fails(409, "DELETE", &format!("/orders/{first}/lines/{line}"), None).contains("lines can only be removed"));

    server.act(&format!("/orders/{first}/fulfil?at={}", at("09:08")));
    let refunded = server.act(&format!("/orders/{first}/refund?at={}", at("09:30")));
    assert_eq!(refunded["status"], json!("refunded"));
    let payments = refunded["payments"].as_array().expect("payments");
    assert_eq!((int(&payments[1], "amount_cents"), int(&payments[1], "refund_of")), (-460, int(&payments[0], "id")));
    let reversal = &entries(&server, DAY, "refund")[0];
    assert_eq!(
        lines(reversal),
        expected(&[("1010", 0, 460), ("4000", 425, 0), ("2000", 35, 0)]),
        "everything but the cost of goods, reversed"
    );
    assert!(server.fails(409, "POST", &format!("/orders/{first}/refund"), None).contains("refunded"));

    server.ok("PATCH", &format!("/menu/items/{}", shop.croissant), Some(json!({ "active": false })));
    let croissant = Some(json!({ "at": at("10:00"), "lines": [{ "menu_item_id": shop.croissant }] }));
    assert!(server.fails(409, "POST", "/orders", croissant).contains("not on the menu"));
    let menu = server.get("/menu");
    assert_eq!(menu["categories"].as_array().map(Vec::len), Some(1), "the bakery has nothing on the menu: {menu:#}");
    assert_eq!(server.get("/menu?all=true")["categories"].as_array().map(Vec::len), Some(2));
    assert_eq!(server.get("/ledger/check")["ok"], json!(true));
    finish(server, &folder);
}

/// Promotion codes, their dates and minimums, the tax on a discounted order,
/// and spending points down to an order that costs nothing.
#[test]
fn promotions_and_points() {
    let folder = scratch("promotions");
    let server = Server::start(&folder.join("coffee.rdb"));
    let shop = shop(&server);
    let large = json!({ "menu_item_id": shop.latte, "size": "large" });
    let croissant = json!({ "menu_item_id": shop.croissant });

    let receipt = order(&server, json!({ "customer_id": shop.maya, "at": at("08:00"), "lines": [large, croissant] }));
    let first = id(&receipt);
    // 10% of 900 is 90. The taxed latte carries 525/900 of the discount (52), so the tax is
    // on 473: 39.02, so 39.
    let welcome = server.post(&format!("/orders/{first}/promotion"), json!({ "code": "Welcome10" }));
    assert_eq!((int(&welcome, "discount_cents"), int(&welcome, "tax_cents"), int(&welcome, "total_cents")), (90, 39, 849));
    let morning = server.post(&format!("/orders/{first}/promotion"), json!({ "code": "morning" }));
    assert_eq!((int(&morning, "discount_cents"), int(&morning, "total_cents")), (100, 839), "a second code replaces the first");
    let croissant_line = morning["lines"][1]["id"].as_i64().expect("the croissant's line");
    let smaller = server.ok("DELETE", &format!("/orders/{first}/lines/{croissant_line}"), None);
    assert_eq!(int(&smaller, "discount_cents"), 0, "below the 800 cent minimum the code gives nothing");
    assert_eq!(smaller["promotion_code"], json!("MORNING"), "and it stays on the order for when it qualifies again");
    assert_eq!(int(&server.post(&format!("/orders/{first}/lines"), croissant.clone()), "discount_cents"), 100);

    assert!(server
        .fails(409, "POST", &format!("/orders/{first}/redeem"), Some(json!({ "points": 100 })))
        .contains("does not have that many points"));
    assert!(server
        .fails(409, "POST", &format!("/orders/{first}/redeem"), Some(json!({ "points": 50 })))
        .contains("redeem_points % 100 = 0"));
    assert_eq!(pay_by_card(&server, first, "08:05", 0)["points"]["earned"], json!(8), "a point a dollar after the discount");

    let big = order(
        &server,
        json!({ "customer_id": shop.maya, "at": at("09:00"), "lines": [{ "menu_item_id": shop.latte, "size": "large", "quantity": 10 }] }),
    );
    assert_eq!(pay_by_card(&server, id(&big), "09:01", 0)["points"]["balance"], json!(50 + 8 + 52));

    // 100 points take 500 cents off, which is more than a small latte, so the order is free.
    let free = order(
        &server,
        json!({ "customer_id": shop.maya, "at": at("10:00"), "redeem_points": 100, "lines": [{ "menu_item_id": shop.latte, "size": "small" }] }),
    );
    assert_eq!((int(&free, "discount_cents"), int(&free, "tax_cents"), int(&free, "total_cents")), (425, 0, 0));
    let paid = server.post(&format!("/orders/{}/pay", id(&free)), json!({ "at": at("10:01"), "payments": [] }));
    assert_eq!(paid["points"], json!({ "earned": 0, "redeemed": 100, "reversed": 0, "balance": 10 }));

    let maya = server.get(&format!("/customers/{}", shop.maya));
    let history: Vec<i64> = maya["points_history"].as_array().expect("history").iter().map(|entry| int(entry, "balance")).collect();
    assert_eq!(history, [50, 58, 110, 10], "a running balance after each entry");
    assert_eq!((maya["points"].clone(), maya["favourite"]["name"].clone(), maya["visits"].clone()), (json!(10), json!("Latte"), json!(3)));

    let anonymous = Some(json!({ "at": at("11:00"), "redeem_points": 100, "lines": [large] }));
    assert!(server.fails(409, "POST", "/orders", anonymous).contains("CHECK constraint failed"), "points need a customer");
    server.post(
        "/promotions",
        json!({ "code": "SPRING", "kind": "percent", "value": 20, "starts_on": "2030-04-01", "ends_on": "2030-04-30" }),
    );
    let spring = Some(json!({ "at": at("11:00"), "promotion_code": "spring", "lines": [large] }));
    assert!(server.fails(409, "POST", "/orders", spring).contains("runs from 2030-04-01 to 2030-04-30"));
    server.fails(404, "POST", "/orders", Some(json!({ "at": at("11:00"), "promotion_code": "NOPE", "lines": [large] })));
    let used = server.get("/promotions");
    let morning_row = used.as_array().and_then(|all| all.iter().find(|p| p["code"] == "MORNING").cloned()).expect("MORNING");
    assert_eq!((morning_row["orders"].clone(), morning_row["discount_given_cents"].clone()), (json!(1), json!(100)));
    assert_eq!(server.get("/ledger/check")["ok"], json!(true));
    finish(server, &folder);
}

/// A delivery moves the average cost; paying the supplier; waste; a stock
/// count; and the checks that the stock and the books agree.
#[test]
fn stock_purchases_waste_and_counts() {
    let folder = scratch("stock");
    let server = Server::start(&folder.join("coffee.rdb"));
    shop(&server);

    // 1000 g at 22,000 a gram and 1000 g more for 30 cents a gram make 2000 g at 26,000.
    let delivery = server.post(
        "/purchases",
        json!({ "supplier": "Roaster", "invoice": "R-2", "at": at("10:00"), "lines": [{ "ingredient_id": 1, "quantity": 1000, "cost_cents": 3000 }] }),
    );
    let beans = inventory_row(&server, "Espresso beans");
    assert_eq!((int(&beans, "on_hand"), int(&beans, "unit_cost_micros")), (2000, 26000));
    assert_eq!(lines(&entries(&server, DAY, "purchase")[1]), expected(&[("1200", 3000, 0), ("2200", 0, 3000)]));
    let duplicate = json!({ "supplier": "Roaster", "invoice": "R-2", "at": at("10:05"), "lines": [{ "ingredient_id": 1, "quantity": 1, "cost_cents": 1 }] });
    assert!(server.fails(409, "POST", "/purchases", Some(duplicate)).contains("UNIQUE"));

    let pay = format!("/purchases/{}/pay?at={}", id(&delivery), at("11:00"));
    assert!(server.act(&pay)["paid_at"].is_string());
    assert_eq!(lines(&entries(&server, DAY, "supplier_payment")[0]), expected(&[("2200", 3000, 0), ("1020", 0, 3000)]));
    assert!(server.fails(409, "POST", &pay, None).contains("UNIQUE"), "the journal's UNIQUE (source, source_id) stops a second payment");

    // 200 ml of milk at 0.11 cents is 22 cents.
    let waste = server.post("/inventory/waste", json!({ "ingredient_id": 2, "quantity": 200, "note": "out of date", "at": at("12:00") }));
    assert_eq!((int(&waste, "change"), int(&waste, "cost_cents")), (-200, -22));
    assert_eq!(lines(&entries(&server, DAY, "waste")[0]), expected(&[("5100", 22, 0), ("1200", 0, 22)]));

    // 10 g of beans short (26 cents) and one cup over (9 cents): 17 cents lost.
    let count = server.post(
        "/inventory/count",
        json!({ "at": at("17:00"), "counts": [{ "ingredient_id": 1, "counted": 1990 }, { "ingredient_id": 4, "counted": 101 }, { "ingredient_id": 3, "counted": 5000 }] }),
    );
    assert_eq!(count, json!({ "movements": 2, "value_change_cents": -17 }), "the oat milk was right, so it has no movement");
    assert_eq!(lines(&entries(&server, DAY, "count")[0]), expected(&[("5150", 17, 0), ("1200", 0, 17)]));
    let after: Vec<i64> =
        server.get("/inventory/1/movements").as_array().expect("movements").iter().map(|m| int(m, "on_hand_after")).collect();
    assert_eq!(after, [1000, 2000, 1990]);

    // Ten croissants thrown away at 1.15 each is 1150 cents, and leaves none: at the reorder level.
    server.post("/inventory/waste", json!({ "ingredient_id": 5, "quantity": 10, "note": "unsold", "at": at("18:00") }));
    let inventory = server.get(&format!("/inventory?day={DAY}"));
    assert_eq!(inventory[0]["name"], json!("Croissants"), "the low ones come first: {inventory:#}");
    assert_eq!(inventory[0]["low"], json!(1));
    let check = server.get("/ledger/check");
    assert_eq!(check["ok"], json!(true), "{check:#}");
    assert_eq!(check["inventory"]["ledger_cents"], json!(7000 + 3000 - 22 - 17 - 1150));
    finish(server, &folder);
}

/// Answers one row of `GET /inventory` by the ingredient's name.
///
/// @param server - the running server
/// @param name - the ingredient
fn inventory_row(server: &Server, name: &str) -> Value {
    let all = server.get(&format!("/inventory?day={DAY}"));
    all.as_array().and_then(|rows| rows.iter().find(|row| row["name"] == name).cloned()).unwrap_or_else(|| panic!("no {name} in {all:#}"))
}

/// A day with shifts, tips, cash and cards is closed: tips shared to the
/// cent, the drawer counted, the cash banked, the cards settled, and the day
/// locked. Then the three statements agree.
#[test]
fn closing_a_day() {
    let folder = scratch("close");
    let server = Server::start(&folder.join("coffee.rdb"));
    let shop = shop(&server);
    server.act(&format!("/staff/{}/clock-in?at={}", shop.ana, at("06:00")));
    server.act(&format!("/staff/{}/clock-in?at={}", shop.ben, at("07:00")));
    assert!(server.fails(409, "POST", &format!("/staff/{}/clock-in?at={}", shop.ana, at("07:30")), None).contains("UNIQUE"));

    let small = order(&server, json!({ "at": at("08:00"), "lines": [{ "menu_item_id": shop.latte, "size": "small" }] }));
    pay_by_card(&server, id(&small), "08:01", 100);
    let croissant = order(&server, json!({ "at": at("08:10"), "lines": [{ "menu_item_id": shop.croissant }] }));
    let cash = json!({ "at": at("08:11"), "payments": [{ "method": "cash", "tip_cents": 25, "tendered_cents": 500 }] });
    assert_eq!(server.post(&format!("/orders/{}/pay", id(&croissant)), cash)["payments"][0]["change_cents"], json!(100));
    let large = order(&server, json!({ "at": at("09:00"), "lines": [{ "menu_item_id": shop.latte, "size": "large" }] }));
    pay_by_card(&server, id(&large), "09:01", 51);
    let open = order(&server, json!({ "at": at("10:00"), "lines": [{ "menu_item_id": shop.latte, "size": "large" }] }));

    let counted = Some(json!({ "counted_cash_cents": 20200, "at": at("18:00") }));
    assert!(server.fails(409, "POST", &format!("/days/{DAY}/close"), counted.clone()).contains("tickets 4"));
    server.act(&format!("/orders/{}/cancel?at={}", id(&open), at("10:05")));
    server.act(&format!("/staff/{}/clock-out?at={}", shop.ana, at("14:00")));
    server.act(&format!("/staff/{}/clock-out?at={}", shop.ben, at("12:30")));

    // 176 cents of tips over 480 and 330 minutes: 104.30 and 71.70. The whole cents make 175,
    // and the one left goes to the larger remainder, Ben's.
    let tips: Vec<(i64, i64)> = server
        .get(&format!("/reports/tips?day={DAY}"))
        .as_array()
        .expect("tips")
        .iter()
        .map(|t| (int(t, "minutes_worked"), int(t, "tip_cents")))
        .collect();
    assert_eq!(tips, [(480, 104), (330, 72)]);

    // The drawer: a 200.00 float, 3.75 and a 0.25 tip in cash, less 1.76 of tips paid out,
    // is 202.24. 202.00 is counted: 24 cents short. The cards took 4.60 + 1.00 + 5.68 + 0.51
    // = 11.79, and the fee is 2.90% of that, 34 cents.
    let z = server.post(&format!("/days/{DAY}/close"), counted.clone().expect("a body"));
    assert_eq!(
        z["close"],
        json!({ "business_day": DAY, "closed_at": at("18:00"), "counted_cash_cents": 20200, "expected_cash_cents": 20224,
                "over_short_cents": -24, "deposit_cents": 200, "card_settled_cents": 1179, "card_fee_cents": 34 })
    );
    assert_eq!(
        (
            int(&z["orders"], "orders_paid"),
            int(&z["orders"], "orders_cancelled"),
            int(&z["orders"], "gross_sales_cents"),
            int(&z["orders"], "takings_cents")
        ),
        (3, 1, 375 + 425 + 525, 460 + 375 + 568)
    );
    assert_eq!(int(&z["orders"], "average_ticket_cents"), 468, "1403 / 3 = 467.67");
    assert_eq!(z["payments"], json!({ "cash_cents": 375, "card_cents": 1028, "tips_cents": 176, "refunded_cents": 0, "payments": 3 }));
    let books = z["books"].as_array().expect("books");
    let debits: i64 = books.iter().map(|row| int(row, "debit_cents")).sum();
    assert_eq!(debits, books.iter().map(|row| int(row, "credit_cents")).sum::<i64>(), "the day's entries balance: {books:?}");
    assert_eq!(lines(&entries(&server, DAY, "tips")[0]), expected(&[("2100", 176, 0), ("1000", 0, 176)]));
    assert_eq!(lines(&entries(&server, DAY, "cash_count")[0]), expected(&[("5300", 24, 0), ("1000", 0, 24)]));
    assert_eq!(lines(&entries(&server, DAY, "card_settlement")[0]), expected(&[("1020", 1145, 0), ("5200", 34, 0), ("1010", 0, 1179)]));

    let late = Some(json!({ "at": at("19:00"), "lines": [{ "menu_item_id": shop.croissant }] }));
    assert!(server.fails(409, "POST", "/orders", late).contains("that business day is closed"));
    assert!(server.fails(409, "POST", &format!("/days/{DAY}/close"), counted).contains("already closed"));
    let drawer = server.get(&format!("/accounts/1000/ledger?from={DAY}&to={DAY}"));
    assert_eq!(
        (drawer["opening_balance_cents"].clone(), drawer["closing_balance_cents"].clone()),
        (json!(0), json!(20000)),
        "the float is left"
    );

    // Cost of goods: the small latte 75 (40 + 26 + 9), the croissant 115, the large latte 128.
    let income = server.get(&format!("/reports/income-statement?from={DAY}&to={DAY}"))["totals"].clone();
    assert_eq!(
        income,
        json!({ "sales_cents": 1325, "discounts_cents": 0, "net_revenue_cents": 1325, "cost_of_goods_cents": 318, "gross_profit_cents": 1007,
                "other_expenses_cents": 58, "net_income_cents": 949, "gross_margin_percent": 76.0 })
    );
    let sheet = server.get(&format!("/reports/balance-sheet?as_of={DAY}"))["totals"].clone();
    assert_eq!(sheet, json!({ "assets_cents": 128027, "liabilities_cents": 7078, "equity_cents": 120949, "balances": true }));
    let trial = server.get(&format!("/reports/trial-balance?as_of={DAY}"));
    let total = trial.as_array().and_then(|rows| rows.last().cloned()).expect("a total row");
    assert_eq!((total["code"].clone(), total["debit_cents"].clone()), (json!("total"), total["credit_cents"].clone()));
    assert_eq!(server.get("/ledger/check")["ok"], json!(true));
    finish(server, &folder);
}

/// The seed's week of trading, read through every report, and still there
/// after a restart.
#[test]
fn reports_on_a_seeded_week() {
    let folder = scratch("seeded");
    let db = folder.join("coffee.rdb");
    let seeded = std::process::Command::new(PROGRAM)
        .args(["seed", "--db", &db.to_string_lossy(), "--until", "2026-09-28"])
        .output()
        .expect("the seed runs");
    assert!(seeded.status.success(), "{}", String::from_utf8_lossy(&seeded.stderr));
    let summary: Value = serde_json::from_slice(&seeded.stdout).expect("the seed prints JSON");
    assert_eq!((summary["check"].clone(), summary["balance_sheet"]["balances"].clone()), (json!(true), json!(true)), "{summary:#}");
    let server = Server::start(&db);

    let sales = server.get("/reports/sales?from=2026-09-20&to=2026-09-27");
    let days = sales.as_array().expect("days");
    assert_eq!(days.len(), 8, "one row a day, the day before trading began included");
    assert_eq!((days[0]["orders"].clone(), days[0]["change_cents"].clone()), (json!(0), Value::Null));
    let net: i64 = days.iter().map(|day| int(day, "net_sales_cents")).sum();
    assert_eq!(int(&days[7], "running_cents"), net);
    let last_seven: i64 = days[1..].iter().map(|day| int(day, "net_sales_cents")).sum();
    assert_eq!(int(&days[7], "average_7_days_cents"), (last_seven as f64 / 7.0).round() as i64);

    let items = server.get("/reports/items?from=2026-09-21&to=2026-09-27");
    let ranked = items["items"].as_array().expect("items");
    assert!(ranked.windows(2).all(|pair| int(&pair[0], "revenue_cents") >= int(&pair[1], "revenue_cents")), "{items:#}");
    assert_eq!(ranked.last().map(|item| item["cumulative_percent"].clone()), Some(json!(100.0)));
    let tops = server.get("/reports/items?from=2026-09-21&to=2026-09-27&top=1")["items"].clone();
    assert_eq!(tops.as_array().map(|all| all.iter().all(|item| item["rank_in_category"] == 1)), Some(true));
    assert_eq!(tops.as_array().map(Vec::len), Some(2), "the best seller of each of the two categories: {tops:#}");

    let hours = server.get("/reports/hourly?day=2026-09-26");
    let hours = hours.as_array().expect("hours");
    assert_eq!((hours.len(), hours[0]["hour"].clone()), (13, json!("06:00")));
    let orders: i64 = hours.iter().map(|hour| int(hour, "orders")).sum();
    let z = server.get("/days/2026-09-26");
    assert_eq!(orders, int(&z["orders"], "orders_paid"));
    assert!(hours.iter().any(|hour| hour["bar"].as_str().map(str::len) == Some(30)), "the busiest hour has the full bar");
    assert!(z["close"].is_object() && z["tip_payouts"].as_array().map(Vec::len) == Some(3), "{z:#}");
    let top: Vec<i64> = z["top_items"].as_array().expect("top items").iter().map(|item| int(item, "sold")).collect();
    assert!(top.windows(2).all(|pair| pair[0] >= pair[1]), "best sellers, most first: {top:?}");

    let queue = server.get("/orders/queue?now=2026-09-27T18:00:00Z");
    let places: Vec<i64> = queue.as_array().expect("queue").iter().map(|entry| int(entry, "place")).collect();
    assert_eq!(places, [1, 2], "the last day ends with two orders still to make: {queue:#}");
    let customers = server.get("/customers");
    assert_eq!(customers.as_array().map(Vec::len), Some(6));
    assert_eq!(customers[0]["spend_rank"], json!(1));
    assert_eq!(server.get("/reports/margins")[0]["rank_on_menu"], json!(1));
    assert_eq!(server.get("/ledger/check")["ok"], json!(true));

    let trial = server.get("/reports/trial-balance?as_of=2026-09-27");
    drop(server);
    let server = Server::start(&db);
    assert_eq!(server.get("/reports/trial-balance?as_of=2026-09-27"), trial, "the books are in the file, not in the process");
    finish(server, &folder);
}

/// Requests the service refuses, and which part of the program refuses each.
#[test]
fn bad_requests_are_refused_with_a_reason() {
    let folder = scratch("refusals");
    let server = Server::start(&folder.join("coffee.rdb"));
    let shop = shop(&server);
    let small = json!({ "menu_item_id": shop.latte, "size": "small" });

    assert!(server.fails(400, "POST", "/orders", Some(json!({ "at": "yesterday", "lines": [small] }))).contains("is not a time"));
    assert!(server.fails(400, "POST", "/orders", Some(json!({ "colour": "red" }))).contains("unknown field"));
    server.fails(400, "GET", "/orders/abc", None);
    server.fails(404, "GET", "/orders/999", None);
    server.fails(404, "GET", "/nothing/here", None);
    server.fails(404, "POST", "/orders", Some(json!({ "at": at("08:00"), "lines": [{ "menu_item_id": shop.latte, "size": "medium" }] })));
    server.fails(
        404,
        "POST",
        "/orders",
        Some(json!({ "at": at("08:00"), "lines": [{ "menu_item_id": shop.latte, "size": "small", "modifiers": [99] }] })),
    );
    let none = Some(json!({ "at": at("08:00"), "lines": [{ "menu_item_id": shop.latte, "size": "small", "quantity": 0 }] }));
    assert!(server.fails(409, "POST", "/orders", none).contains("quantity > 0"));

    let item = |sku: &str, price: i64| json!({ "category": "Coffee", "sku": sku, "name": "Mocha", "prices": { "small": price } });
    assert!(server.fails(409, "POST", "/menu/items", Some(item("MOCHA", -5))).contains("price_cents >= 0"));
    assert!(server.fails(409, "POST", "/menu/items", Some(item("NO SPACES", 400))).contains("GLOB"));
    assert!(server.fails(409, "POST", "/menu/items", Some(item("LATTE", 400))).contains("UNIQUE"));
    let orphan = json!({ "category": "Coffee", "sku": "MOCHA", "name": "Mocha", "prices": { "small": 450 },
                         "recipe": { "large": [{ "ingredient_id": 1, "quantity": 18 }] } });
    assert!(server.fails(409, "POST", "/menu/items", Some(orphan)).contains("FOREIGN KEY"), "a recipe for a size that has no price");
    server.fails(404, "GET", "/menu/items/99", None);

    let promotion = |value: i64, ends: &str| json!({ "code": "X", "kind": "percent", "value": value, "starts_on": DAY, "ends_on": ends });
    assert!(server.fails(409, "POST", "/promotions", Some(promotion(150, DAY))).contains("value <= 100"));
    assert!(server.fails(409, "POST", "/promotions", Some(promotion(10, "2030-01-01"))).contains("ends_on >= starts_on"));
    assert!(server.fails(409, "POST", "/promotions", Some(promotion(10, "2030-06-31"))).contains("date(ends_on)"));
    assert!(server.fails(409, "POST", "/customers", Some(json!({ "name": "Noor", "email": "not an email" }))).contains("LIKE"));
    assert!(server.fails(409, "POST", "/customers", Some(json!({ "name": "Maya again", "email": "MAYA@example.COM" }))).contains("UNIQUE"));
    assert!(server.fails(409, "POST", "/staff/1/clock-out", None).contains("not clocked in"));

    let unbalanced =
        json!({ "memo": "typo", "lines": [{ "account": "1020", "debit_cents": 100 }, { "account": "3000", "credit_cents": 90 }] });
    assert!(server.fails(409, "POST", "/journal", Some(unbalanced)).contains("must be equal"));
    let unknown =
        json!({ "memo": "typo", "lines": [{ "account": "9999", "debit_cents": 100 }, { "account": "3000", "credit_cents": 100 }] });
    assert!(server.fails(409, "POST", "/journal", Some(unknown)).contains("FOREIGN KEY"));
    assert!(server.fails(400, "GET", "/reports/sales?from=2030-01-02&to=2030-01-01", None).contains("range"));
    assert_eq!(server.get("/ledger/check")["ok"], json!(true), "nothing refused left anything behind");
    finish(server, &folder);
}
