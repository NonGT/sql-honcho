-- Run by docker/up.sh with sqlcmd as sa, only when `app` doesn't exist yet.
CREATE DATABASE app;
GO
CREATE DATABASE analytics;
GO
CREATE LOGIN honcho WITH PASSWORD = 'Honcho_test1', CHECK_POLICY = OFF, DEFAULT_DATABASE = app;
GO
USE analytics;
CREATE USER honcho FOR LOGIN honcho;
ALTER ROLE db_owner ADD MEMBER honcho;
GO
USE app;
CREATE USER honcho FOR LOGIN honcho;
ALTER ROLE db_owner ADD MEMBER honcho;
GO

CREATE TABLE dbo.users (
    id INT IDENTITY PRIMARY KEY,
    email NVARCHAR(255) NOT NULL,
    active BIT NOT NULL DEFAULT 1
);

CREATE TABLE dbo.orders (
    id INT IDENTITY PRIMARY KEY,
    user_id INT NOT NULL REFERENCES dbo.users (id),
    amount DECIMAL(12, 2) NOT NULL
);

INSERT INTO dbo.users (email) VALUES (N'a@example.com'), (N'ข@example.com');
INSERT INTO dbo.orders (user_id, amount) VALUES (1, 12.50), (1, 7.25), (2, 100.00);
GO

CREATE VIEW dbo.active_users AS SELECT id, email FROM dbo.users WHERE active = 1;
GO

CREATE PROCEDURE dbo.refresh_stats
AS
BEGIN
    SET NOCOUNT ON;
    SELECT COUNT(*) AS orders FROM dbo.orders;
END;
GO

CREATE FUNCTION dbo.order_total(@p_user INT) RETURNS DECIMAL(12, 2)
AS
BEGIN
    RETURN (SELECT COALESCE(SUM(amount), 0) FROM dbo.orders WHERE user_id = @p_user);
END;
GO
