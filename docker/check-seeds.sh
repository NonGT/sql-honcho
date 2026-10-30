#!/usr/bin/env bash
# Check each running test database has the shared seed, reachable as the
# honcho user. Pass service names to check only some.
set -euo pipefail
cd "$(dirname "$0")"

services=("$@")
[ ${#services[@]} -eq 0 ] && services=(mysql mariadb postgres mssql)
pw=Honcho_test1
failed=0

expect() {
  local name=$1 want=$2 got
  got=$(cat | tr -d '[:space:]')
  if [ "$got" = "$want" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name: expected $want, got '$got'"
    failed=1
  fi
}

objects="'users','orders','active_users','refresh_stats','order_total'"

for s in "${services[@]}"; do
  case $s in
    mysql | mariadb)
      client=mysql
      [ "$s" = mariadb ] && client=mariadb
      q() { docker compose exec -T "$s" "$client" -uhoncho -p"$pw" -N -B app -e "$1" 2>/dev/null; }
      q "SELECT (SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'app')
           + (SELECT COUNT(*) FROM information_schema.routines WHERE routine_schema = 'app')" \
        | expect "$s objects" 5
      q "SELECT email FROM users WHERE id = 2" | expect "$s thai text" "ข@example.com"
      q "SHOW DATABASES LIKE 'analytics'" | expect "$s analytics db" analytics
      ;;
    postgres)
      q() { docker compose exec -T postgres psql -U honcho -d app -tAc "$1"; }
      q "SELECT (SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'public')
           + (SELECT COUNT(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
              WHERE n.nspname = 'public' AND p.proname IN ('refresh_stats', 'order_total'))" \
        | expect "postgres objects" 5
      q "SELECT email FROM users WHERE id = 2" | expect "postgres thai text" "ข@example.com"
      q "SELECT COUNT(*) FROM pg_database WHERE datname = 'analytics'" | expect "postgres analytics db" 1
      ;;
    mssql)
      q() {
        docker compose exec -T mssql /opt/mssql-tools18/bin/sqlcmd -C -S localhost \
          -U honcho -P "$pw" -d app -h -1 -W -Q "SET NOCOUNT ON; $1"
      }
      q "SELECT COUNT(*) FROM sys.objects WHERE name IN ($objects)" | expect "mssql objects" 5
      q "SELECT email FROM dbo.users WHERE id = 2" | expect "mssql thai text" "ข@example.com"
      q "SELECT COUNT(*) FROM sys.databases WHERE name = 'analytics'" | expect "mssql analytics db" 1
      ;;
    *)
      echo "unknown service $s"
      exit 2
      ;;
  esac
done

exit $failed
