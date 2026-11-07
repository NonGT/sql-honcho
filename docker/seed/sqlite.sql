-- SQLite has no server, routines or second database: the tests build a
-- file from this, with the tables and the view in main.
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    email TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT 1
);

CREATE TABLE orders (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users (id),
    amount NUMERIC(12, 2) NOT NULL,
    placed_at DATETIME
);
CREATE INDEX orders_user ON orders (user_id);

INSERT INTO users (email) VALUES ('a@example.com'), ('ข@example.com');
INSERT INTO orders (user_id, amount) VALUES (1, 12.50), (1, 7.25), (2, 100.00);

CREATE VIEW active_users AS SELECT id, email FROM users WHERE active;

CREATE TRIGGER orders_placed AFTER INSERT ON orders
WHEN NEW.placed_at IS NULL
BEGIN
    UPDATE orders SET placed_at = CASE WHEN 1 THEN datetime('now') END WHERE id = NEW.id;
END;
