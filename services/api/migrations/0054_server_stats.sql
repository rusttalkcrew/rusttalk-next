-- Server statistics (#214). `view_server_stats` (2097152) for the owner and admin roles.
UPDATE roles SET permissions = permissions | 2097152 WHERE id IN ('owner', 'admin');
