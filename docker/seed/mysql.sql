-- Shared by MySQL and MariaDB. Runs as root against the `app` database.
CREATE DATABASE IF NOT EXISTS analytics;
GRANT ALL PRIVILEGES ON analytics.* TO 'honcho'@'%';

USE app;

CREATE TABLE users (
    id INT PRIMARY KEY AUTO_INCREMENT,
    email VARCHAR(255) NOT NULL,
    active BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE orders (
    id INT PRIMARY KEY AUTO_INCREMENT,
    user_id INT NOT NULL,
    amount DECIMAL(12, 2) NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users (id)
);

INSERT INTO users (email) VALUES ('a@example.com'), ('ข@example.com');
INSERT INTO orders (user_id, amount) VALUES (1, 12.50), (1, 7.25), (2, 100.00);

CREATE VIEW active_users AS SELECT id, email FROM users WHERE active;

DELIMITER //

CREATE PROCEDURE refresh_stats()
BEGIN
    SELECT COUNT(*) AS orders FROM orders;
END //

CREATE FUNCTION order_total(p_user INT) RETURNS DECIMAL(12, 2)
READS SQL DATA
BEGIN
    RETURN (SELECT COALESCE(SUM(amount), 0) FROM orders WHERE user_id = p_user);
END //

DELIMITER ;
