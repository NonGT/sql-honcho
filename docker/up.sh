#!/usr/bin/env bash
# Start the test databases and seed SQL Server. Pass service names to start
# only some of them, e.g. `docker/up.sh mysql mariadb`.
set -euo pipefail
cd "$(dirname "$0")"

docker compose up -d --wait "$@"

if [ $# -eq 0 ] || [[ " $* " == *" mssql "* ]]; then
  sqlcmd() {
    docker compose exec -T mssql /opt/mssql-tools18/bin/sqlcmd \
      -C -S localhost -U sa -P Honcho_test1 -b -h -1 -W "$@"
  }
  if [ "$(sqlcmd -Q "SET NOCOUNT ON; SELECT COUNT(*) FROM sys.databases WHERE name = 'app'" | tr -d '[:space:]')" = 0 ]; then
    sqlcmd -i /seed/mssql.sql
  fi
fi
