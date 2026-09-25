//! The schema: every table, index, trigger and view, in one statement list.
//!
//! `Store::open` runs this the first time it opens a file with no `orders`
//! table, and never again.
//!
//! ## The tables
//!
//! ```text
//! THE MENU
//! category ──< menu_item ──< menu_price (one row per size) ──< recipe >── ingredient
//! modifier (oat milk, extra shot, vanilla) ── uses or replaces an ingredient
//!
//! THE COUNTER
//! orders ──< order_line ──< order_line_modifier >── modifier
//!   │  └──< payment (cash or card; a refund is a negative payment)
//!   ├── customer ──< loyalty_entry (points earned, redeemed, reversed)
//!   ├── staff ──< shift (clock in, clock out)
//!   └── promotion (a code worth a percent or an amount)
//!
//! THE BACK OFFICE
//! purchase ──< purchase_line >── ingredient ──< stock_movement
//! day_close ──< tip_payout >── staff
//!
//! THE BOOKS
//! account ──< journal_line >── journal_entry
//! ```
//!
//! ## What the database does on its own
//!
//! The service writes orders, lines and payments. Everything that follows
//! from them is written by triggers, in the same transaction:
//!
//! | Trigger | When | What it writes |
//! |---|---|---|
//! | `order_paid` | an order moves from `open` to `paid` | stock used by every line, the sale's journal entry, and the customer's points |
//! | `order_refunded` | a paid order moves to `refunded` | a journal entry that reverses the sale, and the points taken back |
//! | `stock_movement_applies` | any stock movement | the ingredient's `on_hand` |
//! | `order_status_moves_forward` | any status change | refuses one the order lifecycle does not allow |
//! | `order_line_*`, `payment_*`, `orders_*` | writes on a closed order or a closed day | refuses them |
//! | `journal_*_is_permanent` | any update or delete in the books | refuses it: a mistake is corrected by a new entry |
//!
//! A trigger cannot be skipped by a code path that forgot to call it, which is
//! why the books are written this way. A sale cannot be paid without its
//! journal entry, and the stock cannot be counted without its movement.
//!
//! ## Money is an integer number of cents
//!
//! Every amount is an `INTEGER` column whose name ends in `_cents`. A `REAL`
//! cannot hold 0.10 exactly, and a ledger that must balance to the cent cannot
//! be built on sums of numbers that are each slightly wrong. Tax and fees are
//! rates in basis points (825 is 8.25%), and ingredient costs are in millionths
//! of a dollar per gram, millilitre or piece, because a gram of coffee costs
//! less than a cent.
//!
//! ## What inillucent 1.0.30 needed changed
//!
//! Each of these is written the way it is because 1.0.30 answers differently
//! from SQLite 3.53.4. `README.md` lists them, and the bug reports are
//! task-2132 and task-2134.
//!
//! - `RAISE(ABORT, ...)` takes only a string literal. SQLite accepts any
//!   expression, so `'cannot move from ' || OLD.status` would name the status;
//!   here the message is fixed text.
//! - An index on a `VIRTUAL` generated column is refused, so `business_day` on
//!   `orders` and `payment` is `STORED`.
//! - No trigger reads `json_each`: task-2132 item 9.

/// The schema. Run once, when the database has no `orders` table.
pub const SCHEMA: &str = "
-- Settings the service reads when it prices an order or closes a day.
CREATE TABLE setting (
  key    TEXT PRIMARY KEY,
  value  INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO setting (key, value) VALUES
  ('tax_rate_bp', 825),           -- sales tax, 8.25%
  ('card_fee_bp', 290),           -- what the card processor keeps, 2.90%
  ('drawer_float_cents', 20000);  -- cash left in the drawer overnight

-- THE MENU -----------------------------------------------------------------

CREATE TABLE category (
  id        INTEGER PRIMARY KEY,
  name      TEXT NOT NULL UNIQUE COLLATE NOCASE CHECK (length(trim(name)) > 0),
  position  INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE menu_item (
  id           INTEGER PRIMARY KEY,
  category_id  INTEGER NOT NULL REFERENCES category (id),
  sku          TEXT NOT NULL UNIQUE CHECK (sku GLOB '[A-Z]*' AND sku NOT GLOB '*[^A-Z0-9-]*'),
  name         TEXT NOT NULL CHECK (length(trim(name)) > 0),
  taxable      INTEGER NOT NULL DEFAULT 1 CHECK (taxable IN (0, 1)),
  active       INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1))
) STRICT;

-- One row per size an item is sold in. An item sold in one size uses 'regular'.
CREATE TABLE menu_price (
  menu_item_id  INTEGER NOT NULL REFERENCES menu_item (id) ON DELETE CASCADE,
  size          TEXT NOT NULL CHECK (size IN ('small', 'medium', 'large', 'regular')),
  price_cents   INTEGER NOT NULL CHECK (price_cents >= 0),
  PRIMARY KEY (menu_item_id, size)
) STRICT, WITHOUT ROWID;

CREATE TABLE ingredient (
  id                INTEGER PRIMARY KEY,
  name              TEXT NOT NULL UNIQUE COLLATE NOCASE CHECK (length(trim(name)) > 0),
  unit              TEXT NOT NULL CHECK (unit IN ('g', 'ml', 'each')),
  -- Kept equal to the sum of this ingredient's stock movements by the
  -- stock_movement_applies trigger. GET /inventory/reconcile checks it.
  on_hand           INTEGER NOT NULL DEFAULT 0,
  reorder_level     INTEGER NOT NULL DEFAULT 0 CHECK (reorder_level >= 0),
  -- The weighted average cost of what is on hand, in millionths of a dollar
  -- per unit. Every purchase moves it.
  unit_cost_micros  INTEGER NOT NULL DEFAULT 0 CHECK (unit_cost_micros >= 0)
) STRICT;

-- What one item of one size uses. The foreign key names both columns of
-- menu_price's key, so a recipe can only exist for a size that is sold.
CREATE TABLE recipe (
  menu_item_id   INTEGER NOT NULL,
  size           TEXT NOT NULL,
  ingredient_id  INTEGER NOT NULL REFERENCES ingredient (id),
  quantity       INTEGER NOT NULL CHECK (quantity > 0),
  PRIMARY KEY (menu_item_id, size, ingredient_id),
  FOREIGN KEY (menu_item_id, size) REFERENCES menu_price (menu_item_id, size) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

-- A change to a drink. A modifier either adds an ingredient (an extra shot
-- adds 18 g of beans) or replaces one (oat milk replaces whole milk, in the
-- quantity the recipe uses).
CREATE TABLE modifier (
  id                      INTEGER PRIMARY KEY,
  name                    TEXT NOT NULL UNIQUE COLLATE NOCASE,
  price_cents             INTEGER NOT NULL DEFAULT 0 CHECK (price_cents >= 0),
  ingredient_id           INTEGER REFERENCES ingredient (id),
  quantity                INTEGER NOT NULL DEFAULT 0 CHECK (quantity >= 0),
  replaces_ingredient_id  INTEGER REFERENCES ingredient (id),
  CHECK (replaces_ingredient_id IS NULL OR ingredient_id IS NOT NULL)
) STRICT;

-- PEOPLE -------------------------------------------------------------------

CREATE TABLE customer (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL CHECK (length(trim(name)) > 0),
  email       TEXT UNIQUE COLLATE NOCASE CHECK (email IS NULL OR email LIKE '%_@_%'),
  created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
) STRICT;

CREATE TABLE staff (
  id    INTEGER PRIMARY KEY,
  name  TEXT NOT NULL UNIQUE COLLATE NOCASE,
  role  TEXT NOT NULL DEFAULT 'barista' CHECK (role IN ('barista', 'manager'))
) STRICT;

CREATE TABLE shift (
  id          INTEGER PRIMARY KEY,
  staff_id    INTEGER NOT NULL REFERENCES staff (id),
  started_at  TEXT NOT NULL CHECK (started_at IS strftime('%Y-%m-%dT%H:%M:%SZ', started_at)),
  ended_at    TEXT CHECK (ended_at IS NULL OR ended_at IS strftime('%Y-%m-%dT%H:%M:%SZ', ended_at)),
  CHECK (ended_at IS NULL OR ended_at > started_at)
) STRICT;

-- A person can be clocked in once. The index covers only the open shifts, so
-- any number of finished shifts can share a staff_id.
CREATE UNIQUE INDEX shift_one_open ON shift (staff_id) WHERE ended_at IS NULL;

CREATE TABLE promotion (
  id                  INTEGER PRIMARY KEY,
  code                TEXT NOT NULL UNIQUE COLLATE NOCASE,
  kind                TEXT NOT NULL CHECK (kind IN ('percent', 'amount')),
  value               INTEGER NOT NULL CHECK (value > 0),
  min_subtotal_cents  INTEGER NOT NULL DEFAULT 0 CHECK (min_subtotal_cents >= 0),
  starts_on           TEXT NOT NULL CHECK (starts_on IS date(starts_on)),
  ends_on             TEXT NOT NULL CHECK (ends_on IS date(ends_on)),
  CHECK (kind <> 'percent' OR value <= 100),
  CHECK (ends_on >= starts_on)
) STRICT;

-- THE COUNTER ----------------------------------------------------------------

CREATE TABLE orders (
  id              INTEGER PRIMARY KEY,
  opened_at       TEXT NOT NULL CHECK (opened_at IS strftime('%Y-%m-%dT%H:%M:%SZ', opened_at)),
  -- The day the order belongs to, worked out from opened_at. STORED so the
  -- UNIQUE constraint below can index it.
  business_day    TEXT GENERATED ALWAYS AS (date(opened_at)) STORED,
  -- 1, 2, 3 and so on, starting again each day: the number called out at the counter.
  ticket          INTEGER NOT NULL CHECK (ticket > 0),
  channel         TEXT NOT NULL DEFAULT 'counter' CHECK (channel IN ('counter', 'mobile')),
  customer_id     INTEGER REFERENCES customer (id) ON DELETE SET NULL,
  staff_id        INTEGER REFERENCES staff (id),
  status          TEXT NOT NULL DEFAULT 'open'
                  CHECK (status IN ('open', 'paid', 'fulfilled', 'cancelled', 'refunded')),
  promotion_id    INTEGER REFERENCES promotion (id),
  -- Loyalty points spent on this order: 100 points take 500 cents off.
  redeem_points   INTEGER NOT NULL DEFAULT 0 CHECK (redeem_points >= 0 AND redeem_points % 100 = 0),
  subtotal_cents  INTEGER NOT NULL DEFAULT 0 CHECK (subtotal_cents >= 0),
  discount_cents  INTEGER NOT NULL DEFAULT 0,
  tax_cents       INTEGER NOT NULL DEFAULT 0 CHECK (tax_cents >= 0),
  total_cents     INTEGER GENERATED ALWAYS AS (subtotal_cents - discount_cents + tax_cents) STORED,
  paid_at         TEXT,
  fulfilled_at    TEXT,
  cancelled_at    TEXT,
  refunded_at     TEXT,
  UNIQUE (business_day, ticket),
  CHECK (discount_cents BETWEEN 0 AND subtotal_cents),
  CHECK (redeem_points = 0 OR customer_id IS NOT NULL),
  CHECK ((status = 'open') = (paid_at IS NULL AND cancelled_at IS NULL)),
  CHECK (fulfilled_at IS NULL OR fulfilled_at >= paid_at),
  CHECK (refunded_at IS NULL OR refunded_at >= paid_at)
) STRICT;

CREATE INDEX orders_status ON orders (status, paid_at);
CREATE INDEX orders_customer ON orders (customer_id);

CREATE TABLE order_line (
  id                INTEGER PRIMARY KEY,
  order_id          INTEGER NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
  menu_item_id      INTEGER NOT NULL,
  size              TEXT NOT NULL,
  -- The line's modifier ids, sorted and joined with commas, such as '1,2'.
  -- The same drink with the same modifiers added twice is one line of two.
  modifier_key      TEXT NOT NULL DEFAULT '',
  quantity          INTEGER NOT NULL CHECK (quantity > 0),
  -- The price of one, with its modifiers, copied from the menu when the line
  -- was added. A later price change does not reach an order already open.
  unit_price_cents  INTEGER NOT NULL CHECK (unit_price_cents >= 0),
  taxable           INTEGER NOT NULL CHECK (taxable IN (0, 1)),
  line_total_cents  INTEGER GENERATED ALWAYS AS (quantity * unit_price_cents) STORED,
  UNIQUE (order_id, menu_item_id, size, modifier_key),
  FOREIGN KEY (menu_item_id, size) REFERENCES menu_price (menu_item_id, size)
) STRICT;

CREATE TABLE order_line_modifier (
  line_id      INTEGER NOT NULL REFERENCES order_line (id) ON DELETE CASCADE,
  modifier_id  INTEGER NOT NULL REFERENCES modifier (id),
  price_cents  INTEGER NOT NULL,
  PRIMARY KEY (line_id, modifier_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE payment (
  id              INTEGER PRIMARY KEY,
  order_id        INTEGER NOT NULL REFERENCES orders (id),
  method          TEXT NOT NULL CHECK (method IN ('cash', 'card')),
  -- Negative for a refund, which also names the payment it gives back.
  amount_cents    INTEGER NOT NULL,
  tip_cents       INTEGER NOT NULL DEFAULT 0,
  -- The cash handed over. The change is worked out, never stored by hand.
  tendered_cents  INTEGER,
  change_cents    INTEGER GENERATED ALWAYS AS (tendered_cents - amount_cents - tip_cents) VIRTUAL,
  at              TEXT NOT NULL CHECK (at IS strftime('%Y-%m-%dT%H:%M:%SZ', at)),
  business_day    TEXT GENERATED ALWAYS AS (date(at)) STORED,
  refund_of       INTEGER UNIQUE REFERENCES payment (id),
  CHECK (method = 'cash' OR tendered_cents IS NULL),
  CHECK (tendered_cents IS NULL OR tendered_cents >= amount_cents + tip_cents),
  CHECK ((refund_of IS NULL AND amount_cents > 0 AND tip_cents >= 0)
      OR (refund_of IS NOT NULL AND amount_cents < 0 AND tip_cents <= 0))
) STRICT;

CREATE INDEX payment_order ON payment (order_id);
CREATE INDEX payment_day ON payment (business_day, method);

CREATE TABLE loyalty_entry (
  id           INTEGER PRIMARY KEY,
  customer_id  INTEGER NOT NULL REFERENCES customer (id) ON DELETE CASCADE,
  order_id     INTEGER REFERENCES orders (id),
  points       INTEGER NOT NULL CHECK (points <> 0),
  reason       TEXT NOT NULL CHECK (reason IN ('earn', 'redeem', 'refund', 'welcome')),
  at           TEXT NOT NULL
) STRICT;

CREATE INDEX loyalty_customer ON loyalty_entry (customer_id);

-- THE BACK OFFICE --------------------------------------------------------------

CREATE TABLE purchase (
  id            INTEGER PRIMARY KEY,
  supplier      TEXT NOT NULL CHECK (length(trim(supplier)) > 0),
  invoice       TEXT NOT NULL,
  received_at   TEXT NOT NULL CHECK (received_at IS strftime('%Y-%m-%dT%H:%M:%SZ', received_at)),
  total_cents   INTEGER NOT NULL CHECK (total_cents > 0),
  paid_at       TEXT,
  UNIQUE (supplier, invoice)
) STRICT;

CREATE TABLE purchase_line (
  purchase_id    INTEGER NOT NULL REFERENCES purchase (id),
  ingredient_id  INTEGER NOT NULL REFERENCES ingredient (id),
  quantity       INTEGER NOT NULL CHECK (quantity > 0),
  cost_cents     INTEGER NOT NULL CHECK (cost_cents > 0),
  PRIMARY KEY (purchase_id, ingredient_id)
) STRICT, WITHOUT ROWID;

-- Every change to an ingredient's stock, with what it cost. Sales and waste
-- are negative. cost_cents follows the sign of change.
CREATE TABLE stock_movement (
  id             INTEGER PRIMARY KEY,
  ingredient_id  INTEGER NOT NULL REFERENCES ingredient (id),
  change         INTEGER NOT NULL CHECK (change <> 0),
  cost_cents     INTEGER NOT NULL,
  reason         TEXT NOT NULL CHECK (reason IN ('opening', 'sale', 'purchase', 'waste', 'count')),
  order_id       INTEGER REFERENCES orders (id),
  purchase_id    INTEGER REFERENCES purchase (id),
  note           TEXT NOT NULL DEFAULT '',
  at             TEXT NOT NULL
) STRICT;

CREATE INDEX stock_movement_ingredient ON stock_movement (ingredient_id, at);
CREATE INDEX stock_movement_order ON stock_movement (order_id);

-- One row per closed day. Its primary key is also the lock: a second close of
-- the same day is a UNIQUE failure, and the triggers below refuse new orders
-- and payments on a day that has a row here.
CREATE TABLE day_close (
  business_day         TEXT PRIMARY KEY CHECK (business_day IS date(business_day)),
  closed_at            TEXT NOT NULL,
  counted_cash_cents   INTEGER NOT NULL CHECK (counted_cash_cents >= 0),
  expected_cash_cents  INTEGER NOT NULL,
  over_short_cents     INTEGER GENERATED ALWAYS AS (counted_cash_cents - expected_cash_cents) STORED,
  deposit_cents        INTEGER NOT NULL,
  card_settled_cents   INTEGER NOT NULL,
  card_fee_cents       INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE tip_payout (
  business_day    TEXT NOT NULL REFERENCES day_close (business_day),
  staff_id        INTEGER NOT NULL REFERENCES staff (id),
  minutes_worked  INTEGER NOT NULL CHECK (minutes_worked > 0),
  tip_cents       INTEGER NOT NULL CHECK (tip_cents >= 0),
  PRIMARY KEY (business_day, staff_id)
) STRICT, WITHOUT ROWID;

-- THE BOOKS --------------------------------------------------------------------

CREATE TABLE account (
  code  TEXT PRIMARY KEY CHECK (code GLOB '[0-9][0-9][0-9][0-9]'),
  name  TEXT NOT NULL UNIQUE,
  type  TEXT NOT NULL CHECK (type IN ('asset', 'liability', 'equity', 'revenue', 'expense'))
) STRICT, WITHOUT ROWID;

INSERT INTO account (code, name, type) VALUES
  ('1000', 'Cash drawer',         'asset'),
  ('1010', 'Card clearing',       'asset'),
  ('1020', 'Bank',                'asset'),
  ('1200', 'Inventory',           'asset'),
  ('2000', 'Sales tax payable',   'liability'),
  ('2100', 'Tips payable',        'liability'),
  ('2200', 'Accounts payable',    'liability'),
  ('3000', 'Owner equity',        'equity'),
  ('4000', 'Sales',               'revenue'),
  ('4100', 'Discounts',           'revenue'),
  ('5000', 'Cost of goods sold',  'expense'),
  ('5100', 'Waste',               'expense'),
  ('5150', 'Stock count changes', 'expense'),
  ('5200', 'Card fees',           'expense'),
  ('5300', 'Cash over and short', 'expense');

CREATE TABLE journal_entry (
  id            INTEGER PRIMARY KEY,
  business_day  TEXT NOT NULL CHECK (business_day IS date(business_day)),
  posted_at     TEXT NOT NULL,
  -- What wrote the entry, and the row it is about. The UNIQUE constraint
  -- means a sale is posted once, a refund once, a purchase once. A manual
  -- entry has no source row, and NULLs never collide in a UNIQUE constraint.
  source        TEXT NOT NULL CHECK (source IN ('manual', 'sale', 'refund', 'purchase', 'supplier_payment',
                                                'waste', 'count', 'tips', 'cash_count', 'deposit', 'card_settlement')),
  source_id     INTEGER,
  memo          TEXT NOT NULL,
  UNIQUE (source, source_id)
) STRICT;

CREATE INDEX journal_entry_day ON journal_entry (business_day);

CREATE TABLE journal_line (
  id            INTEGER PRIMARY KEY,
  entry_id      INTEGER NOT NULL REFERENCES journal_entry (id),
  account_code  TEXT NOT NULL REFERENCES account (code),
  debit_cents   INTEGER NOT NULL DEFAULT 0 CHECK (debit_cents >= 0),
  credit_cents  INTEGER NOT NULL DEFAULT 0 CHECK (credit_cents >= 0),
  -- Exactly one side has an amount.
  CHECK ((debit_cents = 0) <> (credit_cents = 0))
) STRICT;

CREATE INDEX journal_line_entry ON journal_line (entry_id);
CREATE INDEX journal_line_account ON journal_line (account_code, entry_id);

-- TRIGGERS: keeping the books and the stock -------------------------------------

CREATE TRIGGER stock_movement_applies AFTER INSERT ON stock_movement
BEGIN
  UPDATE ingredient SET on_hand = on_hand + NEW.change WHERE id = NEW.ingredient_id;
END;

CREATE TRIGGER journal_entry_is_permanent BEFORE UPDATE ON journal_entry
BEGIN
  SELECT RAISE(ABORT, 'journal entries are permanent: post a new entry that corrects it');
END;

CREATE TRIGGER journal_entry_cannot_be_deleted BEFORE DELETE ON journal_entry
BEGIN
  SELECT RAISE(ABORT, 'journal entries are permanent: post a new entry that corrects it');
END;

CREATE TRIGGER journal_line_is_permanent BEFORE UPDATE ON journal_line
BEGIN
  SELECT RAISE(ABORT, 'journal lines are permanent: post a new entry that corrects it');
END;

CREATE TRIGGER journal_line_cannot_be_deleted BEFORE DELETE ON journal_line
BEGIN
  SELECT RAISE(ABORT, 'journal lines are permanent: post a new entry that corrects it');
END;

-- TRIGGERS: the order lifecycle -----------------------------------------------

--   open ──> paid ──> fulfilled
--    │         │          │
--    v         └────> refunded <──┘
--  cancelled
CREATE TRIGGER order_status_moves_forward BEFORE UPDATE OF status ON orders
WHEN NOT (OLD.status = NEW.status
       OR (OLD.status = 'open' AND NEW.status IN ('paid', 'cancelled'))
       OR (OLD.status = 'paid' AND NEW.status IN ('fulfilled', 'refunded'))
       OR (OLD.status = 'fulfilled' AND NEW.status = 'refunded'))
BEGIN
  SELECT RAISE(ABORT, 'the order lifecycle does not allow that status change');
END;

CREATE TRIGGER order_line_needs_open_order BEFORE INSERT ON order_line
WHEN (SELECT status FROM orders WHERE id = NEW.order_id) IS NOT 'open'
BEGIN
  SELECT RAISE(ABORT, 'lines can only be added to an open order');
END;

CREATE TRIGGER order_line_frozen_after_open BEFORE UPDATE ON order_line
WHEN (SELECT status FROM orders WHERE id = OLD.order_id) IS NOT 'open'
BEGIN
  SELECT RAISE(ABORT, 'lines can only be changed on an open order');
END;

CREATE TRIGGER order_line_kept_after_open BEFORE DELETE ON order_line
WHEN (SELECT status FROM orders WHERE id = OLD.order_id) IS NOT 'open'
BEGIN
  SELECT RAISE(ABORT, 'lines can only be removed from an open order');
END;

CREATE TRIGGER orders_on_open_day BEFORE INSERT ON orders
WHEN EXISTS (SELECT 1 FROM day_close WHERE business_day = date(NEW.opened_at))
BEGIN
  SELECT RAISE(ABORT, 'that business day is closed');
END;

CREATE TRIGGER payment_on_open_day BEFORE INSERT ON payment
WHEN EXISTS (SELECT 1 FROM day_close WHERE business_day = date(NEW.at))
BEGIN
  SELECT RAISE(ABORT, 'that business day is closed');
END;

CREATE TRIGGER order_redeems_points_it_has BEFORE UPDATE OF redeem_points ON orders
WHEN NEW.redeem_points > 0 AND NEW.customer_id IS NOT NULL
 AND NEW.redeem_points > (SELECT coalesce(sum(points), 0) FROM loyalty_entry WHERE customer_id = NEW.customer_id)
BEGIN
  SELECT RAISE(ABORT, 'the customer does not have that many points');
END;

-- Paying an order: use the stock, post the sale, and move the points. Each
-- statement reads what the one before it wrote: the journal's cost of goods
-- is the sum of the stock movements the first statement made.
CREATE TRIGGER order_paid AFTER UPDATE OF status ON orders
WHEN OLD.status = 'open' AND NEW.status = 'paid'
BEGIN
  -- 1. The stock every line used. A line uses its recipe, except where one of
  --    its modifiers replaces an ingredient, and then every modifier adds its
  --    own ingredient: in the replaced ingredient's quantity when it replaces
  --    one, and in its own quantity when it adds one. The cost is valued at
  --    each ingredient's average cost, rounded to the cent.
  INSERT INTO stock_movement (ingredient_id, change, cost_cents, reason, order_id, at)
  SELECT u.ingredient_id, -sum(u.quantity), -((sum(u.quantity) * i.unit_cost_micros + 5000) / 10000),
         'sale', NEW.id, NEW.paid_at
  FROM (
    SELECT r.ingredient_id AS ingredient_id, r.quantity * l.quantity AS quantity
    FROM order_line l
    JOIN recipe r ON r.menu_item_id = l.menu_item_id AND r.size = l.size
    WHERE l.order_id = NEW.id
      AND NOT EXISTS (SELECT 1 FROM order_line_modifier lm JOIN modifier m ON m.id = lm.modifier_id
                      WHERE lm.line_id = l.id AND m.replaces_ingredient_id = r.ingredient_id)
    UNION ALL
    SELECT m.ingredient_id, coalesce(r.quantity, m.quantity) * l.quantity
    FROM order_line l
    JOIN order_line_modifier lm ON lm.line_id = l.id
    JOIN modifier m ON m.id = lm.modifier_id
    LEFT JOIN recipe r ON r.menu_item_id = l.menu_item_id AND r.size = l.size
                      AND r.ingredient_id = m.replaces_ingredient_id
    WHERE l.order_id = NEW.id AND m.ingredient_id IS NOT NULL
  ) u
  JOIN ingredient i ON i.id = u.ingredient_id
  GROUP BY u.ingredient_id
  HAVING sum(u.quantity) > 0;

  -- 2. The sale's journal entry.
  INSERT INTO journal_entry (business_day, posted_at, source, source_id, memo)
  VALUES (date(NEW.paid_at), NEW.paid_at, 'sale', NEW.id, 'sale, ticket ' || NEW.ticket || ' of ' || NEW.business_day);

  -- 3. Its lines: what came in against what was sold. Rows with no amount
  --    are left out, because a journal line must have an amount on one side.
  INSERT INTO journal_line (entry_id, account_code, debit_cents, credit_cents)
  SELECT e.id, x.account, x.debit, x.credit
  FROM journal_entry e,
       (SELECT 1 AS n, '1000' AS account,
               (SELECT coalesce(sum(amount_cents + tip_cents), 0) FROM payment
                WHERE order_id = NEW.id AND method = 'cash') AS debit, 0 AS credit
        UNION ALL
        SELECT 2, '1010', (SELECT coalesce(sum(amount_cents + tip_cents), 0) FROM payment
                           WHERE order_id = NEW.id AND method = 'card'), 0
        UNION ALL SELECT 3, '4100', NEW.discount_cents, 0
        UNION ALL SELECT 4, '4000', 0, NEW.subtotal_cents
        UNION ALL SELECT 5, '2000', 0, NEW.tax_cents
        UNION ALL SELECT 6, '2100', 0, (SELECT coalesce(sum(tip_cents), 0) FROM payment WHERE order_id = NEW.id)
        UNION ALL SELECT 7, '5000', (SELECT -coalesce(sum(cost_cents), 0) FROM stock_movement
                                     WHERE order_id = NEW.id AND reason = 'sale'), 0
        UNION ALL SELECT 8, '1200', 0, (SELECT -coalesce(sum(cost_cents), 0) FROM stock_movement
                                        WHERE order_id = NEW.id AND reason = 'sale')) x
  WHERE e.source = 'sale' AND e.source_id = NEW.id AND (x.debit > 0 OR x.credit > 0)
  ORDER BY x.n;

  -- 4. Points: spent ones first, then one point per whole dollar paid for
  --    goods after the discount.
  INSERT INTO loyalty_entry (customer_id, order_id, points, reason, at)
  SELECT NEW.customer_id, NEW.id, -NEW.redeem_points, 'redeem', NEW.paid_at
  WHERE NEW.customer_id IS NOT NULL AND NEW.redeem_points > 0;

  INSERT INTO loyalty_entry (customer_id, order_id, points, reason, at)
  SELECT NEW.customer_id, NEW.id, (NEW.subtotal_cents - NEW.discount_cents) / 100, 'earn', NEW.paid_at
  WHERE NEW.customer_id IS NOT NULL AND NEW.subtotal_cents - NEW.discount_cents >= 100;
END;

-- Refunding an order: a journal entry with every line of the sale on the other
-- side, except the cost of goods. The drinks were made, so the stock they used
-- stays used. The points the order earned are taken back and the ones it spent
-- are returned. The service writes the refund payments before this runs.
CREATE TRIGGER order_refunded AFTER UPDATE OF status ON orders
WHEN NEW.status = 'refunded' AND OLD.status IN ('paid', 'fulfilled')
BEGIN
  INSERT INTO journal_entry (business_day, posted_at, source, source_id, memo)
  VALUES (date(NEW.refunded_at), NEW.refunded_at, 'refund', NEW.id,
          'refund, ticket ' || NEW.ticket || ' of ' || NEW.business_day);

  INSERT INTO journal_line (entry_id, account_code, debit_cents, credit_cents)
  SELECT r.id, l.account_code, l.credit_cents, l.debit_cents
  FROM journal_line l
  JOIN journal_entry s ON s.id = l.entry_id AND s.source = 'sale' AND s.source_id = NEW.id
  JOIN journal_entry r ON r.source = 'refund' AND r.source_id = NEW.id
  WHERE l.account_code NOT IN ('5000', '1200')
  ORDER BY l.id;

  INSERT INTO loyalty_entry (customer_id, order_id, points, reason, at)
  SELECT customer_id, order_id, -points, 'refund', NEW.refunded_at
  FROM loyalty_entry
  WHERE order_id = NEW.id AND reason IN ('earn', 'redeem');
END;

-- VIEWS ------------------------------------------------------------------------

-- Every account with its balance on its normal side: assets and expenses grow
-- with debits, liabilities, equity and revenue with credits.
CREATE VIEW account_balance AS
SELECT a.code, a.name, a.type,
       coalesce(sum(l.debit_cents), 0) AS debit_cents,
       coalesce(sum(l.credit_cents), 0) AS credit_cents,
       CASE WHEN a.type IN ('asset', 'expense')
            THEN coalesce(sum(l.debit_cents), 0) - coalesce(sum(l.credit_cents), 0)
            ELSE coalesce(sum(l.credit_cents), 0) - coalesce(sum(l.debit_cents), 0) END AS balance_cents
FROM account a
LEFT JOIN journal_line l ON l.account_code = a.code
GROUP BY a.code;

-- Entries whose debits and credits differ. Every transaction that posts to
-- the books reads this before it commits, and it must be empty.
CREATE VIEW unbalanced_entry AS
SELECT e.id, e.source, e.source_id, e.memo,
       coalesce(sum(l.debit_cents), 0) AS debit_cents,
       coalesce(sum(l.credit_cents), 0) AS credit_cents
FROM journal_entry e
LEFT JOIN journal_line l ON l.entry_id = e.id
GROUP BY e.id
HAVING coalesce(sum(l.debit_cents), 0) <> coalesce(sum(l.credit_cents), 0)
    OR count(l.id) = 0;

CREATE VIEW customer_points AS
SELECT c.id AS customer_id, coalesce(sum(le.points), 0) AS points
FROM customer c
LEFT JOIN loyalty_entry le ON le.customer_id = c.id
GROUP BY c.id;

-- A price for every item and size, with what its recipe costs today.
CREATE VIEW menu_margin AS
SELECT m.id AS menu_item_id, m.sku, m.name, c.name AS category, p.size, p.price_cents,
       coalesce((SELECT (sum(r.quantity * i.unit_cost_micros) + 5000) / 10000
                 FROM recipe r JOIN ingredient i ON i.id = r.ingredient_id
                 WHERE r.menu_item_id = p.menu_item_id AND r.size = p.size), 0) AS cost_cents
FROM menu_item m
JOIN category c ON c.id = m.category_id
JOIN menu_price p ON p.menu_item_id = m.id;

-- An order as a list or a queue shows it, with who and what.
CREATE VIEW order_summary AS
SELECT o.id, o.business_day, o.ticket, o.channel, o.status,
       o.customer_id, c.name AS customer_name, o.staff_id, s.name AS staff_name,
       o.subtotal_cents, o.discount_cents, o.tax_cents, o.total_cents,
       o.opened_at, o.paid_at, o.fulfilled_at,
       coalesce((SELECT sum(quantity) FROM order_line WHERE order_id = o.id), 0) AS items
FROM orders o
LEFT JOIN customer c ON c.id = o.customer_id
LEFT JOIN staff s ON s.id = o.staff_id;
";
