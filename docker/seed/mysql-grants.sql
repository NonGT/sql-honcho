-- MySQL only: without this, routine bodies are hidden from everyone but
-- their definer (root here). MariaDB lets the honcho user read them already.
GRANT SHOW_ROUTINE ON *.* TO 'honcho'@'%';
