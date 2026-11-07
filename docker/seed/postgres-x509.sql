-- Signs in with the client certificate in docker/tls, no password.
CREATE ROLE honcho_x509 LOGIN;
GRANT SELECT ON users TO honcho_x509;
