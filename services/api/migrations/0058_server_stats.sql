-- Server statistics (#214). `view_server_stats` (4194304) for the owner and admin roles.
UPDATE roles SET permissions = permissions | 4194304 WHERE id IN ('owner', 'admin');
