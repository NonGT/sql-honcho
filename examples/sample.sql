-- รายงานยอดขายรายเดือน (monthly sales report)
CREATE OR REPLACE FUNCTION sales_by_month(p_year INT)
RETURNS TABLE (month DATE, total NUMERIC(12, 2), orders BIGINT)
LANGUAGE plpgsql STABLE
AS $$
BEGIN
    RETURN QUERY
    SELECT date_trunc('month', o.created_at)::date AS month,
           SUM(o.amount)                         AS total,
           COUNT(*)                              AS orders
    FROM orders o
    WHERE EXTRACT(YEAR FROM o.created_at) = p_year
      AND o.status <> 'cancelled'
    GROUP BY 1
    ORDER BY 1;
END;
$$;

/* SQL Server style */
DECLARE @CustomerId INT = 42;
SELECT TOP 10 c.[Full Name], c.email, N'สวัสดี' AS greeting
FROM dbo.Customers AS c
WHERE c.id = @CustomerId;

SELECT id, name, email FROM users WHERE id = $1 OR email = :email;
