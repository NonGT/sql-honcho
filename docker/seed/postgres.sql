-- Runs as honcho against the `app` database.
CREATE DATABASE analytics;

CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    email TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE orders (
    id SERIAL PRIMARY KEY,
    user_id INT NOT NULL REFERENCES users (id),
    amount NUMERIC(12, 2) NOT NULL
);

INSERT INTO users (email) VALUES ('a@example.com'), ('ข@example.com');
INSERT INTO orders (user_id, amount) VALUES (1, 12.50), (1, 7.25), (2, 100.00);

CREATE VIEW active_users AS SELECT id, email FROM users WHERE active;

CREATE PROCEDURE refresh_stats()
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM COUNT(*) FROM orders;
END
$$;

CREATE FUNCTION order_total(p_user INT) RETURNS NUMERIC(12, 2)
LANGUAGE sql STABLE
AS $$
    SELECT COALESCE(SUM(amount), 0) FROM orders WHERE user_id = p_user
$$;
